# Running Crosspane (v0)

Crosspane v0 runs one agent per machine (`crosspane-agent`, in the user's desktop session, never as
root) and is driven with `crosspanectl`. There is no settings UI yet; everything below is CLI.

## Build

```sh
cargo build --release -p crosspane-agent -p crosspanectl
```

On the Mac, wrap the agent in a signed app bundle so macOS permissions stick to it across rebuilds
(the signing identity comes from `crosspane.local.toml`):

```sh
scripts/macos/bundle.sh --bin target/release/crosspane-agent --id io.frostdev.crosspane.agent \
    --name Crosspane --out target/macos-bundles --ui-element
```

For the opt-in virtual-display window hiding on the Mac (D7), build with
`--features private-vdisplay` and set `mac_virtual_display = true` (below). Without it, projected
Mac windows stay visible in place (M1 mirror).

## One-time setup

**Mac**, System Settings › Privacy & Security, turn on **Crosspane** under:

- **Local Network**: without it the agent can't reach the other machine at all ("No route to host"
  in the log).
- **Accessibility**: injecting input (E1 target) and moving windows (E2 source).
- **Input Monitoring**: capturing the keyboard and mouse (E1 controller).
- **Screen & System Audio Recording**: capturing windows (E2 source).

`Crosspane.app/Contents/MacOS/Crosspane permissions --request` asks for the last three;
`crosspanectl status` lists what is still missing. Local Network is asked for the first time the
agent connects.

**Linux**: the firewall must allow UDP 47811 (agent) and 47812 (pairing) from the LAN, e.g.
`sudo ufw allow from 192.168.4.0/22 to any port 47811:47812 proto udp`.

## Configuration

`config.toml`, created with defaults on first run, at `~/.config/crosspane/config.toml` (Linux) or
`~/Library/Application Support/Crosspane/config.toml` (Mac):

```toml
name = "desktop"          # what peers see
port = 47811              # UDP; pairing uses port + 1
crossing = true           # E1: the pointer may leave this machine's screen edges
push_to_cross_ms = 0      # hold the pointer at an edge this long before crossing (0–200)
mac_virtual_display = false   # Mac + private-vdisplay build only: hide projected windows (D7)

[[peers]]                 # machines to connect to (both directions work; one side is enough)
addr = "192.168.4.244:47811"
```

## Start and stop

- **Linux** (inside Hyprland): `crosspane-agent run`. For autostart on Omarchy, add
  `o.launch_on_start("crosspane-agent run")` to `~/.config/hypr/autostart.lua`.
- **Mac**: `open Crosspane.app --args run` (it has no Dock icon).
- **Stop** with SIGTERM or Ctrl-C: the agent releases any injected input, puts every parked window
  back and closes its connections. After a crash, the next start does the same from its journals.

## Pair two machines (once)

1. On machine A: `crosspanectl pair listen --allow-input`, then `crosspanectl pair status`. It
   shows a six-digit code.
2. On machine B: `crosspanectl pair join <A's address>:47811 --allow-input`, then
   `crosspanectl pair status`. It shows three candidate codes.
3. On B: `crosspanectl pair pick <N>` for the candidate that matches A's code.
4. On A: `crosspanectl pair confirm yes`.

`--allow-input` lets the other machine control this one's keyboard and mouse; leave it out for
window projection only. Both machines then show the peer in `crosspanectl status`.

## Use

**One keyboard and mouse (E1).**
- Set where the other machine is: `crosspanectl layout <peer> left|right|above|below`.
- Push the pointer past that edge. A banner shows while input is routed to the other machine.
- `crosspanectl release` takes input back at once. `crosspanectl panic` ends every session and
  disarms crossing until `crosspanectl rearm`.

**Project a window (E2).**
- `crosspanectl windows` lists this machine's windows.
- `crosspanectl project <id> <peer>` moves the window into a window on the peer, where it is used
  normally. The app keeps running here, and its window is hidden here while projected.
- `crosspanectl return <projection>` (or closing the projected window on the peer) gives the window
  back.

`crosspanectl status` shows peers, layout, projections, missing permissions and recent notices.
