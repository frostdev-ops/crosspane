# macOS development bundles

Run these on the Mac as the logged-in owner, either over SSH or in a GUI terminal.
The owner must already be logged in with the Apple Development signing key
available in that GUI session. No keychain, TCC database, or persistent launchd
configuration is changed by these scripts.

## Run a command in the GUI session

```sh
scripts/macos/run-in-gui.sh -- /bin/sh -c 'echo hi; echo err >&2; exit 3'
scripts/macos/run-in-gui.sh --timeout 30 -- /usr/bin/true
```

The runner uses `launchctl submit`, which the lead verified works over SSH where
`launchctl asuser` fails. It preserves the caller's working directory, PATH, and
argument boundaries; other environment variables come from launchd. A wrapper
records the command's exit status before exiting successfully to prevent
`submit` from restarting failed commands. The runner prints captured stdout and
stderr to their respective streams, returns that status, and removes the
transient `io.frostdev.crosspane.run.<random>` job and temporary files.
The default timeout is 120 seconds; a timeout returns 124.

## Bundle and sign

```sh
scripts/macos/bundle.sh --bin target/debug/crosspane-agent \
  --id io.frostdev.crosspane.agent --name CrosspaneAgent \
  --out target/macos-bundles --ui-element
```

The default identity is `signing_identity_sha1` from `[macos]` in
`~/src/crosspane/crosspane.local.toml` in the main clone. Override it with
`--identity SHA1`; pass an existing entitlement plist with `--entitlements PLIST`.
Signing runs through the GUI runner using the hardened runtime without a
timestamp. Strict verification runs directly before the signed bundle replaces
the previous bundle in the output directory.

## Build and launch the TCC probe

```sh
scripts/macos/build-tccprobe.sh
app="$PWD/target/macos-bundles/CrosspaneTccProbe.app"
codesign --verify --strict "$app"
codesign -d -r- "$app"
open -W -n --stdout "$PWD/target/macos-bundles/probe.stdout" \
  --stderr "$PWD/target/macos-bundles/probe.stderr" "$app"
cat target/macos-bundles/probe.stdout
```

The probe prints one JSON line with `screen_capture`, `accessibility`, and
`input_monitoring` booleans. Launch the `.app` through `open` so LaunchServices
starts the bundle as its own TCC responsible process, rather than executing its
binary under an SSH shell or terminal. To request grants, the owner runs the
same `open` command with `--args --request` appended and approves Screen Recording,
Accessibility, and Input Monitoring in System Settings. Relaunch to read the
current grants; accessibility prompting is asynchronous. The lead then rebuilds
and relaunches to confirm the grants persist.

## TCC identity

TCC remembers permission grants by the app's code-signing requirement.
An ad-hoc signature can change that requirement when the binary changes.
Using the same bundle ID and stable Apple Development identity keeps the
designated requirement identical across rebuilds even when the cdhash changes.
The owner grants permissions once per bundle ID, and the lead verifies that
those grants survive a rebuild.
