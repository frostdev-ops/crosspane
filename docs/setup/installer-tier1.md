# Tier 1 installer builds

Tier 1 uses local developer artifacts. It does not provide Developer ID signing, notarization,
a disk image, or a download trust service. Building never installs or launches Crosspane.
Run the installer later from a folder in the selected user's home, such as Downloads.
Do not run the Mac installer from a disk image, `/Applications`, or another user's directory;
the existing target admission refuses those locations.

## Linux

The Linux stager packages an already prepared developer-approved input directory; it does not
compile or execute artifacts. The absolute input directory must contain `provenance.json`, its
listed shared libraries in `libraries/`, and these exact members:

```text
bin/crosspane-agent
bin/crosspanectl
bin/crosspane-ui
bin/crosspane-installer
resources/crosspane-agent.service
resources/crosspane-settings.desktop
resources/crosspane-installer.desktop
resources/crosspane-icon.svg
resources/LICENSE
resources/io.frostdev.crosspane.agent.desktop
resources/gnome-shell-extension/extension.js
resources/gnome-shell-extension/metadata.json
resources/gnome-shell-extension/io.frostdev.Crosspane.Shell1.xml
```

The last four members are the GNOME and KDE support files (manifest schema 2). A schema 1
manifest lists only the first nine, which is still a valid Hyprland payload; a GNOME or KDE
session refuses it ("prepared before GNOME and KDE support") rather than half-installing. The
archive always carries all thirteen under schema 2; what a session installs from them is decided
by the session:

| Session | Installed from the four support members |
|---|---|
| Hyprland (uwsm) | nothing |
| KDE Plasma (Wayland) | `~/.local/share/applications/io.frostdev.crosspane.agent.desktop` (the desktop portals refuse the agent's application id without it) |
| GNOME (Wayland) | that desktop entry, and the Shell extension in `~/.local/share/gnome-shell/extensions/crosspane@frostdev.io/` (left out when the Shell is known to be older than version 48, the first the extension supports), which is then added to `org.gnome.shell enabled-extensions` through `gsettings` (every existing entry stays; a failure to do so is a warning). GNOME loads a newly installed extension at the next log-in, and the installer says so. |

Removal takes back exactly what was installed, from the installer's own record
(`~/.local/state/crosspane/installer/desktop-outcome.json`): a changed file is kept, and only this
extension's entry is removed from `enabled-extensions`.

`provenance.json` records schema version 1 or 2, product version, architecture (`x86_64` or `aarch64`),
the 40-character source revision, profile (`release`), and each member's size, SHA-256 and sorted
features. The agent must include `video`. Each declared library has a name and SHA-256.
Use the installer's approved templates: the stager validates the complete placeholder syntax,
ELF architecture, hashes and bounded sizes. Inputs must be ordinary user-owned files without
symlinks or hard links. A fixture or a guessed manifest is not a release input.

Prepare that input from a clean checkout with `scripts/installer/prepare-linux-input.sh`. It
builds the release binaries (the agent with `video`), strips their debug info (the release
profile keeps it, and the stager refuses any file over 64 MiB), copies the approved templates,
records the runtime libraries the binaries link against and writes `provenance.json`. It builds
only and refuses a dirty tree or an existing output:

```sh
scripts/installer/prepare-linux-input.sh "$HOME/build-input"
```

Then, from the Linux checkout, with the prepared input under `/home/<user>/build-input`:

```sh
mkdir -p -m 700 "$HOME/installer-builds"
scripts/test-env.sh \
  scripts/installer/stage-linux.sh \
  "$HOME/build-input" "$HOME/installer-builds/linux-tier1"
scripts/test-env.sh scripts/installer/test-stage-linux.sh
```

The output must not already exist. Keep `payload.tar` and `payload.sha256` together in a folder
under the selected user's home. The Linux installer integration accepts that directory as its
explicit payload source; missing or unreadable inputs leave installation pending without mutation.
For an owner-attended run, launch the built installer with `--payload "$HOME/installer-builds/linux-tier1"`.

## Mac

Use an Apple Silicon Mac with Xcode command-line tools, Rust, Python 3, Homebrew `opus`, and the
existing local Apple Development identity configured under `[macos]` in
`~/src/crosspane/crosspane.local.toml` (`signing_identity_sha1` and `team_id`). No key is read or
exported. The established `bundle.sh`/`run-in-gui.sh` helper makes signing-only jobs in the GUI
keychain context; it does not bootstrap or stop product services.

From the Mac checkout:

```sh
mkdir -p target
cd -P .
scripts/installer/stage-mac.sh "$PWD/target/installer-tier1"
scripts/installer/test-stage-mac.sh
```

The output parent must exist inside this checkout's `target/`, be user-owned, have no symlink
ancestry, and not allow other users to write. The output directory must be fresh. The stager
publishes it only after all validation succeeds; a failed build leaves no partially shipped app.
It builds the release agent with `private-vdisplay,video`, settings and CLI, and the
audio driver's install/remove packages. It bundles and signs libopus with the agent.

The output contains `Crosspane Installer.app`, a build-time `ApprovedInventory.json` sample,
and `SHA256SUMS` covering every app leaf and the sample. The payload is inside
`Crosspane Installer.app/Contents/Resources/payload`; its audio packages are inside
`payload/Crosspane.app/Contents/Resources/audio`. The adjacent JSON is for build inspection only.
Runtime never reads that JSON: the outer installer's signed binary embeds the approved inventory.
The inventory tool hashes the signed payload, sorts its records, extracts each executable's
designated requirement and validates through the unchanged producer validator. Package build
timestamps can vary; deterministic inventory ordering does not promise byte-identical packages.

**Tier 1 limitation:** the payload's mandatory `crosspane-installer` is a separately signed
bootstrap build without an inventory. It is deliberately non-mutating, avoiding a recursive hash
of the binary that embeds its own inventory. Use the downloaded **Crosspane Installer.app** for
install, repair and uninstall; the installed/bootstrap copy cannot perform those operations.
Tier 2 will revisit this arrangement. Keep the outer app and its entire payload together.

After building, copy the outer app into a folder in the selected user's home before an
owner-attended run, with `ditto` so modes and the nested signatures are kept:

```sh
mkdir -p "$HOME/Downloads/Crosspane Tier 1"
ditto "target/installer-tier1/Crosspane Installer.app" \
  "$HOME/Downloads/Crosspane Tier 1/Crosspane Installer.app"
/usr/bin/codesign --verify --deep --strict "$HOME/Downloads/Crosspane Tier 1/Crosspane Installer.app"
```

Copy it locally (as above, or `rsync -a` from the build Mac). An app that arrives with a
quarantine attribute (a browser, AirDrop, Mail) may be opened from a randomized read-only location
(App Translocation) until it is moved in the Finder; the installer refuses that location as "not in
your home folder". Gatekeeper may require the normal user approval for this local Development build;
these scripts never bypass it or change TCC, launch agents, the keychain, or audio devices.
The package test regenerates and compares the inventory, checks its exact bytes in the outer
binary (absent from the bootstrap), checks all hashes/modes and verifies the nested code signature.


## Read-only installer diagnostics

On the machine you want to inspect, run an already built installer against an existing staged
payload:

```sh
crosspane-installer --diagnose --payload /absolute/path/to/payload
```

Each `--diagnose` report includes support/session/runtime/font evidence, owned payload state,
service or LaunchAgent state, and matched agent Status. It starts no GUI, sends only read-only
queries, and never installs, starts, stops, cleans, authorizes, or changes settings. The report is
printed as JSON on standard output. An S issue remains a safety refusal; an E issue is
a visible evidence note; exact dead-runtime recovery is R and observation alone never cleans it.
The script fails on an E hard stop and summarizes S stops separately. Unknown evidence is never
reported as ready. Known incompatibility still prevents the affected startup action.

A Mac development binary without an embedded approved inventory reports the signature-bound
payload and agent observations as S/unknown. For full signed-payload coverage, use a newly staged
installer containing the approved inventory; the runtime payload cannot
supply or replace that trust root. Font failure is E in the report. The GUI still needs at least one
safe, parsable system font to draw its notes.
