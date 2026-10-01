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

[remap]                   # optional: modifier swaps while this machine drives a peer
macbook = "swap-ctrl-gui" # none | swap-ctrl-gui (Ctrl ↔ ⌘/Super) | swap-alt-gui (Alt ↔ ⌘/Super)
```

A remap profile applies while *this* machine's keyboard drives that peer, so set it on the
machine whose keyboard you use. Changes to `config.toml` apply after `crosspanectl restart`.

## Install, start and stop

- **Linux:** `cargo install --locked --path apps/crosspane-agent --root ~/.local` (and the same
  for `apps/crosspanectl`), then run it with the graphical session as a systemd user service:

  ```sh
  install -Dm644 packaging/linux/crosspane-agent.service ~/.config/systemd/user/crosspane-agent.service
  systemctl --user daemon-reload && systemctl --user enable --now crosspane-agent
  ```

  Logs: `journalctl --user -u crosspane-agent`. Without systemd, run `crosspane-agent run`
  inside the Hyprland session.
- **Mac:** `scripts/macos/install-agent.sh` (add `--features private-vdisplay` for the D7
  window hiding) builds, signs and installs `~/Applications/Crosspane.app`, starts it at login
  (LaunchAgent `io.frostdev.crosspane.agent`) and puts `crosspanectl` in `~/.cargo/bin`. Logs:
  `~/Library/Logs/Crosspane/agent.log`. Re-run it to upgrade.
- **Stop** with SIGTERM (`systemctl --user stop crosspane-agent`, or `launchctl bootout
  gui/$(id -u)/io.frostdev.crosspane.agent`): the agent releases any injected input, puts every
  parked window back and closes its connections. After a crash, the next start does the same from
  its journals.
- **Restart** with `crosspanectl restart`. The agent also restarts by itself when macOS
  permissions change, so new grants take effect; if a grant doesn't show up in
  `crosspanectl status`, restart it.

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

**The tray / menu-bar icon** (Waybar on Linux, the menu bar on the Mac) is the main control:
- the status, and **Take input back** while this machine drives another;
- **Show a window of ⟨peer⟩ here** and **Send a window to ⟨peer⟩**;
- **Projected windows**, to give any of them back;
- **Machines**: per machine, its side and what it may do here (control, browse), plus
  **Pair a new machine…** and the pairing steps;
- on the Mac, the missing permissions, each explained; clicking one opens its Settings pane;
- **Stop everything (panic)**, **Restart** and **Quit**.

Everything is also available from the command line:

**One keyboard and mouse (E1).**
- Set where the other machine is: `crosspanectl layout <peer> left|right|above|below`.
- Push the pointer past that edge. A banner shows while input is routed to the other machine.
- `crosspanectl release` takes input back at once. `crosspanectl panic` ends every session and
  disarms crossing until `crosspanectl rearm`.

**Project a window (E2).**
- From the machine that has it: `crosspanectl windows` lists this machine's windows, and
  `crosspanectl project <id> <peer>` moves one to the peer.
- From the machine where you want it: `crosspanectl pick --from <peer>` shows the peer's windows
  in a menu (walker, fuzzel, wofi or rofi on Linux; a list on the Mac) and pulls the chosen one.
  Bind it to a key, e.g. in Hyprland `crosspanectl pick --from macbook`. The scriptable form is
  `crosspanectl windows --from <peer>` and `crosspanectl pull <peer> <id>`.
  - The peer must allow it once: on the peer, `crosspanectl allow <this machine> browse` (or the
    tray's **May browse and pull my windows**).
- The window is used normally where it's shown; the app keeps running at home, hidden there.
- `crosspanectl return <projection>` (or closing the projected window) gives the window back.

**Machines and permissions.**
- `crosspanectl allow <peer> input|share|browse|present [--off]` grants or withdraws one thing.
- `crosspanectl forget <peer>` unpairs a machine and ends its connection at once.
- `crosspanectl restart` restarts the agent (the agent also restarts itself when macOS
  permissions change).

`crosspanectl status` shows peers, layout, projections (with received frames), missing
permissions and recent notices.
