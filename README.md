<p align="center">
  <img src="assets/brand/crosspane-banner-logo.png" alt="Crosspane — Your computers. One workspace." width="960">
</p>

<p align="center">
  <strong>Your computers. One workspace.</strong><br>
  One keyboard and mouse across your Mac, Windows PC and Hyprland desktop.<br>
  Bring windows, clipboard content and sound along, too.
</p>

<p align="center">
  <a href="docs/running.md">Get started</a> ·
  <a href="#what-you-can-do">Explore the features</a> ·
  <a href="#platforms-and-current-status">Platform support</a> ·
  <a href="https://github.com/frostdev-ops/crosspane/issues">Report an issue</a>
</p>

<p align="center">
  <a href="https://github.com/frostdev-ops/crosspane/actions/workflows/ci.yml"><img src="https://github.com/frostdev-ops/crosspane/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <img src="https://img.shields.io/badge/status-development%20preview-89cbd5" alt="Development preview">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPL--3.0--or--later-89cbd5" alt="GPL-3.0-or-later"></a>
</p>

## Make the whole desk yours

Your Mac has one application. Your Windows PC or Linux desktop has another. Crosspane brings them into the same working space: move your pointer across the edge of a display and keep typing on the next computer, or bring a single application window over to the screen where you want it.

The application keeps running on its own machine. Its projected window appears on the other one, where you can use it with your keyboard and mouse. You keep each computer's applications and environment, while choosing where the work is in front of you.

Crosspane is part of [Frostdev](https://frostdev.io), alongside [Rimeward](https://github.com/frostdev-ops/rimeward) and [Frostsim](https://github.com/frostdev-ops/frostsim).

**Crosspane is a development preview.** The workspace is currently version `0.0.0`. Shared input, window projection and dragging, clipboard sharing, speaker sharing, pairing, settings and installer flows are implemented. Mac and Hyprland are the established development targets; Windows support is implemented as a preview, with distribution signing and broader native validation still in progress. The setup paths documented here are source builds and local developer installers.

## What you can do

| | In Crosspane |
| --- | --- |
| **Use one keyboard and mouse** | Arrange your computers' displays on a shared canvas. Cross a touching edge to control the next machine. |
| **Bring a window over** | Show an individual application window from another computer here, or send one of yours there. The application continues running at its source. |
| **Drag a window across** | Drag a window toward a neighbouring machine's screen edge to project it there, where the platform supports the gesture. Dragging a projection back can return the original. |
| **Share your clipboard** | Copy text or images on one machine and paste on another. Reading and offering clipboard content have separate per-peer permissions and are off by default. |
| **Use another machine's speakers** | Send sound to a paired computer with its permission. Windows source audio captures the applications whose windows are projected. |
| **Arrange your desk** | Drag machines in the settings app's layout editor, with displays sized in millimetres, and apply the layout. |
| **Choose what a peer may do** | Pair explicitly, then grant or withdraw input, sharing, browsing, presentation, clipboard and audio permissions per machine. |
| **Remap your modifiers** | Use per-peer profiles for Ctrl ↔ Command / Super or Alt ↔ Command / Super. |
| **Stay in control** | Take input back from the tray, return a projected window, or stop every session with the panic command. |
| **Use the command line** | Pair, arrange displays, browse windows, project, return, and inspect status with `crosspanectl`. |
| **Install, repair and remove** | Use the installer wizard for local payload installation, startup integration, permissions, pairing and layout, plus repair and removal. |

### A window, wherever you need it

Pick a window from the other computer using the tray menu, the settings app, or `crosspanectl pick --from <peer>`. The peer must first allow you to browse its windows. You can also send a window from its source with `crosspanectl project <id> <peer>`.

Source-window behavior depends on the platform:

- **Hyprland:** the window can be parked on a headless twin output, with mirroring as a fallback.
- **macOS:** the default is mirroring, so the original stays visible. Hiding it requires the opt-in `private-vdisplay` build and `mac_virtual_display = true`, which use the private `CGVirtualDisplay` API.
- **Windows:** twin-display parking is implemented through the Crosspane IddCx driver. Without an available driver, projection falls back to mirroring the visible original. Driver deployment and signing remain part of the preview's setup work.

Close the projected window or choose **Return** to give it back. If a connection drops, held keys and buttons are released immediately. The projected window keeps its last picture for a 20-second reconnect grace period, then returns to its source if the connection has not recovered. The [running guide](docs/running.md) explains projection, dragging, permissions and recovery.

### Your machines, paired directly

Crosspane connects peer to peer over IP, including a LAN, direct Ethernet, or USB4 / Thunderbolt networking. Pairing requires comparing a code on the two machines. Connections use QUIC with TLS 1.3 and authenticated peer identities.

Each computer runs its own agent in your desktop session. The settings app talks to that local agent; the agent handles connections, input, projection, clipboard and audio. Crosspane respects lock state, macOS Secure Event Input and Windows secure-desktop boundaries. Clipboard and speaker access require explicit per-peer grants.

## Platforms and current status

| Platform | Current scope |
| --- | --- |
| **Apple Silicon Mac** | Input sharing, projection, clipboard, CoreAudio speaker sharing, menu-bar controls and installer flows are implemented. Requires macOS permissions and signed app bundles; see the [macOS build helpers](scripts/macos/README.md). |
| **Linux / Hyprland** | Input sharing, projection, window dragging, clipboard, PipeWire speaker sharing, tray controls and installer flows are implemented. Requires a Hyprland graphical session and native libraries. The installer targets a uwsm-managed session. |
| **Windows** | Development preview: input sharing, projection, dragging, clipboard, WASAPI playback and projected-app audio, tray controls, settings and installer flows are implemented. Signed distribution and broader attended validation are unfinished. |
| **GNOME / KDE and other Linux desktops** | No platform backend is implemented yet. |

Windows source and destination fixtures, native smoke scripts and platform models are included in the repository. A successful build or portable model test does not establish that every native desktop scenario works.

## Getting started

1. **Build and install on each machine.** Follow [Running Crosspane](docs/running.md) for Mac/Hyprland setup and use the platform build notes below. Development installers need staged payloads; an ordinary Cargo build alone does not produce a distributable installer.
2. **Pair them.** Open the settings app's **Pairing** tab on each machine. Open a pairing window on one, scan and join on the other, compare the code, then confirm.
3. **Arrange the displays.** In **Layout**, place the machines as they sit on your desk and apply. Enable the input permission for shared keyboard and mouse control.
4. **Bring a window along.** Use **Windows** in settings or the tray / menu-bar window picker. Grant browsing on the source when you want to pick its windows from another machine. Enable clipboard or speaker permissions separately when you want those features.

### Build from source

Install the toolchain pinned in [`rust-toolchain.toml`](rust-toolchain.toml), currently Rust `1.98.1`, and your platform's native build dependencies. Build the agent, command-line client, settings app and installer shell:

```sh
cargo build --locked --release -p crosspane-agent -p crosspanectl -p crosspane-ui -p crosspane-installer
```

| Platform | Build prerequisites |
| --- | --- |
| **Linux** | `pkg-config`, CMake, Clang/libclang, libxkbcommon, Opus and PipeWire development libraries. Vulkan support is used for rendering. Add FFmpeg 9 development libraries for the `video` feature. |
| **macOS** | Xcode command-line tools, Python 3, Homebrew Opus and a stable signing identity for app bundles. For direct Cargo builds, set `OPUS_LIB_DIR` as shown in the [running guide](docs/running.md#build). |
| **Windows** | The MSVC Rust toolchain, Visual Studio C++ Build Tools and CMake. In PowerShell, run `. .\scripts\windows\dev-env.ps1` before building to configure CMake and the prebuilt NASM objects. Building the IddCx driver also requires the Windows Driver Kit. |

For H.264 motion streaming on Linux or Windows, build the agent with the `video` feature:

```sh
cargo build --locked --release -p crosspane-agent --features video
```

Linux uses FFmpeg with hardware encoding where available; Windows uses Media Foundation. macOS uses VideoToolbox. Without an available video codec, projection uses lossless tiles. GPU capture and rendering paths are implemented, with availability depending on the platform and hardware.

### Build an installer

The installer handles installation, repair, removal and startup integration. Its usable payload inventories are generated by the platform staging scripts:

- **Linux and macOS:** follow [Tier 1 installer builds](docs/setup/installer-tier1.md) for payload preparation, app bundling and signing. macOS speaker sharing uses the Crosspane audio component; distribution notarization is separate from these developer builds.
- **Windows:** [stage-windows.ps1](scripts/installer/stage-windows.ps1) builds the installer kit and embeds its payload inventories. It takes a driver directory containing `CrosspaneIdd.inf`, `CrosspaneIdd.cat` and `CrosspaneIdd.dll`; it does not sign those inputs. Driver and firewall installation use a separate elevated setup helper, while the agent runs in the normal user session.

### A few useful commands

```sh
crosspanectl status                         # connections, permissions, layout, projections
crosspanectl layout macbook right           # put a paired machine to the right
crosspanectl pick --from macbook            # choose one of its windows to show here
crosspanectl allow macbook clipboard.read   # let that peer read my clipboard when pasting there
crosspanectl allow macbook speaker          # let that peer play through my speakers
crosspanectl release                        # take keyboard and mouse control back
crosspanectl panic                          # stop all sessions and disarm edge crossing
crosspanectl rearm                          # enable edge crossing again
```

Replace `macbook` with the name of your paired machine. The corresponding `clipboard.write` grant lets that peer offer its clipboard here; add `--off` to withdraw a grant.

## Current limitations

- **Development preview:** platform code and installers are present, but signed distribution and broader hardware/session validation are still in progress. Windows precision-touchpad and AltGr behavior, lock-state synchronization and attended secure-desktop checks have remaining limitations.
- **Window projection:** capture, hiding, cursor shapes and drag gestures vary by platform. Notifications on a parked Hyprland output may appear in its projection. Hyprland cursor capture is off by default after a compositor crash observed during development.
- **Clipboard:** text and PNG images only, with limits of 1 MiB and 16 MiB respectively. Access is opt-in and blocked when the session is locked or unknown.
- **Audio:** speaker sharing only; remote microphones are not implemented. Windows projected-app capture requires OS build `20348` or later and does not mute the application locally. The Mac audio driver currently serves one peer at a time.
- **Video:** full-screen motion over lossless tiles can consume substantial bandwidth. H.264 availability depends on the build, native codecs and hardware.

See the [running guide](docs/running.md#known-limitations-v0) for configuration and further platform limits. The banner above is brand artwork, rather than an application screenshot. Follow [GitHub issues](https://github.com/frostdev-ops/crosspane/issues) for current development.

## Built with Rust

Crosspane separates its OS-independent types, protocol, security, input routing, media, and engine from thin platform adapters. QUIC carries the peer connections; winit and wgpu host projected windows; egui / eframe power the settings app and installer. Native adapters handle macOS, Hyprland and Windows permissions, capture, input, audio, clipboard and window management.

To work on the project, read the [running guide](docs/running.md) and install the pinned Rust toolchain. Use [scripts/test-env.sh](scripts/test-env.sh) to keep local checks isolated from your desktop session:

```sh
scripts/test-env.sh cargo fmt --check
scripts/test-env.sh cargo clippy --workspace --all-targets --locked -- -D warnings
scripts/test-env.sh cargo nextest run --workspace --locked --no-tests=pass
```

The [CI workflow](.github/workflows/ci.yml) also checks Rust documentation, dependency policy, layering and Windows cross-compilation. Native desktop scenarios require separate platform runs. Hyprland compositor tests skip unless the [nested test session](scripts/hypr-nested/README.md) enables them explicitly.

## Documentation

| | Start here |
| --- | --- |
| **Install and use** | [Running Crosspane](docs/running.md) |
| **Prepare a Mac** | [macOS build helpers](scripts/macos/README.md) |
| **Build a Linux or Mac installer** | [Tier 1 installer builds](docs/setup/installer-tier1.md) |
| **Stage a Windows installer** | [Windows staging script](scripts/installer/stage-windows.ps1) |
| **Inspect the Windows twin driver** | [IddCx driver source](drivers/windows-idd/) |
| **Run isolated platform tests** | [Nested Hyprland test session](scripts/hypr-nested/README.md) |
| **Use the brand assets** | [Brand guide and asset inventory](assets/brand/README.md) |

## License

[GPL-3.0-or-later](LICENSE). Distributed versions and modifications carry the same freedoms for their recipients.
