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

The agent asks macOS for whatever is missing when it starts (at most once a day), so the system's
"Crosspane would like to…" dialogs appear by themselves; `crosspanectl request-permissions` asks
again at any time.

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

- **Linux:** `cargo install --locked --path apps/crosspane-agent --root ~/.local --features video`
  (and the same, without the feature, for `apps/crosspanectl` and `apps/crosspane-ui`), then run
  it with the graphical session as a systemd user service:

  ```sh
  install -Dm644 packaging/linux/crosspane-agent.service ~/.config/systemd/user/crosspane-agent.service
  systemctl --user daemon-reload && systemctl --user enable --now crosspane-agent
  ```

  Logs: `journalctl --user -u crosspane-agent`. Without systemd, run `crosspane-agent run`
  inside the Hyprland session.
- **Mac:** `scripts/macos/install-agent.sh --features video` (add `private-vdisplay`, i.e.
  `--features private-vdisplay,video`, for the D7 window hiding) builds, signs and installs `~/Applications/Crosspane.app` (with the settings app
  inside), starts it at login (LaunchAgent `io.frostdev.crosspane.agent`) and puts `crosspanectl`
  in `~/.cargo/bin`. Logs: `~/Library/Logs/Crosspane/agent.log`. Re-run it to upgrade.
- **Arch:** `packaging/arch/PKGBUILD` builds all three and installs the user unit and a
  "Crosspane Settings" launcher entry.
- **Stop** with SIGTERM (`systemctl --user stop crosspane-agent`, or `launchctl bootout
  gui/$(id -u)/io.frostdev.crosspane.agent`): the agent releases any injected input, puts every
  parked window back and closes its connections. After a crash, the next start does the same from
  its journals.
- **Restart** with `crosspanectl restart`. The agent also restarts by itself when macOS
  permissions change, so new grants take effect; if a grant doesn't show up in
  `crosspanectl status`, restart it.

## Pair two machines (once)

The **Pairing** tab of the settings app does this with buttons: on A, **Open pairing window**;
on B, **Scan**, then **Join** next to A's name and click the code A shows; on A, **Confirm**.
From the command line:

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
- **Settings…**, which opens the settings app;
- **Stop everything (panic)**, **Restart** and **Quit**.

**The settings app** (`crosspane-ui`, or **Settings…** in the tray) has four tabs:
- **Machines**: each paired machine, whether it's connected, and what it may do here (control,
  share, browse, present), plus **Forget…**;
- **Layout**: every machine's displays to scale, in millimetres. Drag a machine to where its
  screens are on your desk (it snaps to the others' edges) and **Apply**; the pointer crosses
  where displays of different machines touch;
- **Pairing**: pair a new machine, as above;
- **Windows**: send this machine's windows to another one, show another machine's windows here,
  and give projected windows back.

Everything is also available from the command line:

**One keyboard and mouse (E1).**
- Set where the other machine is: drag it in the settings app's **Layout** tab, or
  `crosspanectl layout <peer> left|right|above|below`, or place one display exactly with
  `crosspanectl place <machine> <display id> <x mm> <y mm>`.
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
- If the connection drops, projected windows wait for 20 s: the proxy keeps its last picture and
  the window stays hidden at home, and both resume when the machines reconnect. Held keys and
  buttons are released at once. After 20 s the window goes home.

**Machines and permissions.**
- `crosspanectl allow <peer> input|share|browse|present [--off]` grants or withdraws one thing.
- `crosspanectl forget <peer>` unpairs a machine and ends its connection at once.
- `crosspanectl revoke <peer>` is for a lost or stolen machine: it is forgotten here, and every
  other paired machine is told to forget it too (now, or when it next connects). It can only come
  back through a fresh pairing.
- `crosspanectl restart` restarts the agent (the agent also restarts itself when macOS
  permissions change).

`crosspanectl status` shows peers, layout, projections (with received frames), missing
permissions and recent notices.

## Known limitations (v0)

- **Notifications** that your notification daemon shows on the output where a projected window is
  parked appear in that projected window (they're captured with it). Pinning the daemon to a real
  output (e.g. mako's `output=`) avoids it.
- **Cursor shape:** over a projected window the cursor takes the source app's shape (text beam,
  hand, resize arrows) when the source can see it: on a Mac source (needs Screen Recording).
  Windows from Hyprland show the destination's default arrow: Hyprland 0.56.2's cursor capture
  crashed the compositor once (02 §3.3), so it is off. `CROSSPANE_HYPR_CURSORS=1` in the agent's
  environment turns it on, and even then it only covers apps that draw their own cursor (GTK3,
  Firefox, Xwayland), not GTK4, Qt 6 or Chromium.
- **If Hyprland restarts** (e.g. after a crash), the agent notices that its instance is gone,
  exits and is started again by systemd on the new one.
- **Picking windows:** a peer's windows must be allowed once (`crosspanectl allow <peer> browse` on
  that peer). Window titles from a Mac need Screen Recording there.
- **Video:** H.264 needs a build with `--features video` (FFmpeg 9 on Linux; built in on the Mac);
  without it everything is lossless tiles, which is fine on a wired LAN but heavy for full-screen
  video over Wi-Fi.

