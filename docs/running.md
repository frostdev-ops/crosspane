# Running Crosspane (v0)

Crosspane v0 runs one agent per machine (`crosspane-agent`, in the user's desktop session, never as
root) and is driven with `crosspanectl` or the settings app (`crosspane-ui`, opened from the tray /
menu-bar item). Everything below is shown with the CLI.

## Build

On macOS, install system Opus and select it before a direct Cargo build:

```sh
brew install opus
export OPUS_LIB_DIR=${OPUS_LIB_DIR:-$(brew --prefix opus)}
```

The Mac build and install helpers set this variable automatically. Homebrew Opus
and `OPUS_LIB_DIR` remain build prerequisites. As of `f15c311`,
`scripts/macos/bundle.sh` embeds all recursively linked Homebrew dylibs (including
libopus) in `Crosspane.app/Contents/Frameworks`, rewrites their load commands to
bundle-relative paths, and signs the libraries with the bundle identity for the
hardened runtime. The resulting app does not require installed Homebrew Opus at
runtime.

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

By default the agents on a network find each other with mDNS and reconnect by themselves.
`CROSSPANE_DISCOVERY=0` in the agent's environment turns that off: nothing is advertised or
browsed, no discovered address is ever dialled, and `crosspanectl pair scan` finds nothing. The
agent then connects only to the `[[peers]]` addresses and to `crosspanectl dial` addresses; it logs
"discovery off" at start. Test scripts that run agents set it, so they never contact the agents of
the machine they run on.

## Install, start and stop

- **Linux:** `cargo install --locked --path apps/crosspane-agent --root ~/.local --features video`
  (and the same, without the feature, for `apps/crosspanectl` and `apps/crosspane-ui`), then run
  it with the graphical session as a systemd user service:

  ```sh
  sed -e "s|{{agent_executable}}|$HOME/.local/bin/crosspane-agent|" -e '/^Environment={{/d' \
      -e '/^# Installer payload template/d' packaging/linux/crosspane-agent.service \
    | install -Dm644 /dev/stdin ~/.config/systemd/user/crosspane-agent.service
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
- **Locked key store:** if the OS key store (Secret Service or Keychain) is still locked when the
  agent starts at login, `crosspane-agent run` waits for it, retrying every 2 s and logging once a
  minute, and SIGTERM still stops it while it waits.

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
  and give projected windows back. The app uses Crosspane's dark arctic artwork and glass panels.
  For review without an agent, run `CROSSPANE_UI_DEMO=1 cargo run -p crosspane-ui`.
  To capture a tab and exit, run
  `CROSSPANE_UI_SCREENSHOT=/tmp/crosspane-layout.png CROSSPANE_UI_TAB=layout cargo run -p crosspane-ui`;
  tab names are `machines`, `layout`, `pairing` and `windows`. Screenshot mode automatically uses
  the demo model. Both review modes stay disconnected from the real agent socket.

Everything is also available from the command line:

**One keyboard and mouse (E1).**
- Set where the other machine is: drag it in the settings app's **Layout** tab, or
  `crosspanectl layout <peer> left|right|above|below`, or place one display exactly with
  `crosspanectl place <machine> <display id> <x mm> <y mm>`.
- Push the pointer past that edge. A banner shows while input is routed to the other machine.
- `crosspanectl release` takes input back at once. `crosspanectl panic` ends every session and
  disarms crossing until `crosspanectl rearm`.

**Windows (preview).** High-resolution and horizontal wheel deltas retain their wheel units.
Pixel-only scrolling uses an approximate conversion based on the target's wheel settings and
20 logical pixels per line or character; applications may scroll by different amounts. Windows
precision-touchpad capture and AltGr interoperability still need attended native validation.
Lock-state snapshots are unknown in this preview, so crossing from Windows leaves the target's
lock states unchanged. A target refuses to guess a requested toggle when its own state is unknown.
Win+L locks the local Windows machine. Ctrl-Alt-Del opens Windows security locally by design;
interact with the security or lock screen on that machine. Remote secure-desktop control is
unsupported, and Crosspane closes its input gate there.

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

**Use a projected desktop window from the same keyboard (Hyprland).** When this machine
controls a peer and the pointer moves into a projection of its own twin-parked window, input goes
home into that window. The notice names the window and **Ctrl+Shift+Alt+Escape**; the peer stays
connected. Push past an available window edge to resume controlling the peer, or press the chord
to end control and return to the physical display. The tray marks the home projection.

Entry needs a current proxy placement, recent physical pointer motion, confirmed exit strips,
settled injectors and a verified release shortcut. It waits during a resize and refuses a
fullscreen proxy with no usable exit. Move into the proxy before typing: typing from outside a
proxy that still has focus does not enter home. Mirror parking and Mac sources keep their usual
behaviour. The peer's cursor stays at the entry point during home; cursor painting is deferred.

The agent installs the home shortcut only for a home transaction, with the description
`crosspane-home-release`, and verifies/removes it on exit, startup and shutdown. Config reloads
are watched and a missing shortcut is reinstalled within the one-second housekeeping interval.
An existing binding on the chord prevents entry. Cleanup leaves a foreign-only binding intact;
if ours and another binding coexist, or ownership cannot be read, input stays fenced until
cleanup is confirmed. Reload the Hyprland config when the notice asks. Don't add a permanent
binding on this chord: it would prevent the verified home shortcut from being installed.

`crosspanectl --json status` includes `home: { projection, bind_installed }`; the projection is
`null` outside home. `bind_installed` is the last verified presence of our shortcut. Unknown
presence is reported as false while the cleanup fence remains in force. There is no new config
setting: the shortcut follows the engine's default release chord. The command uses the agent's
runtime directory and the `crosspanectl` beside its executable. Paths containing apostrophes,
control characters or the Lua string terminator are refused for installation.

**Validation for the lead (WP-2.43e).** The nested `scripts/e2e/e1e2-nested.sh` checks twin parking,
confirmed strips, compositor placements, startup/shutdown ownership and a direct home-bind round
trip. It also requires a proxy-motion report observed without local capture motion and no home
notice. The frozen engine API does not expose whether prevalidation or corroboration rejected the
report. A virtual pointer cannot supply the capture's physical relative motion, so this run cannot
validate home entry or establish the exact rejection reason. Run it through `scripts/lead/impl-env.sh`, passing the parent display only as
`CROSSPANE_PARENT_WAYLAND_DISPLAY`; all clients and IPC calls address the named nests. The
`CROSSPANE_TWIN_BACKEND=wayland` override requires `CROSSPANE_NESTED_HYPR=1` and a nonempty
monitor list on that IPC endpoint containing only `WAYLAND-*` outputs. Other or unknown outputs
retain the headless backend; this permits only the first twin stand-in in a nest. The existing
`e1-nested.sh` changes the outer desktop and remains a lead-run regression check.

The lead's live prerequisite uses one window briefly with a physical mouse and keyboard:

- Entry notice shows the chord.
- `hyprctl cursorpos` is on the twin.
- Four strips are on it (`hyprctl layers`).
- `binds -j` lists the bind.
- Typing reaches W.
- Push past an edge: B receives motion, A logs the exit, the bind is gone.
- Press the chord while home: A warps to the fallback and B reports "control ended".
- Re-enter.
- `hyprctl reload` while home: the bind is reinstalled within 1 s.

Record status before, during and after home, the bind verification line and the first placement
report. Check entry with focus-following enabled and disabled, and check overlapping Mac proxies
with one not the key window. If the nested twin prerequisite fails, report Mirror-only coverage;
twin checks then remain live-only. No live check is run by the work-package implementer.

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

## Audio (speakers, v0)

A machine can play its sound on a paired machine's speakers (D8; design in `docs/wp/AUDIO-v0.md`).
Speakers only: microphones are not shared yet.

- **Use it:** each paired machine that has an audio backend shows a virtual output called
  "⟨peer⟩ speakers" (on Linux, a PipeWire sink; on the Mac, the Crosspane audio driver's "Crosspane
  speakers", once the owner has installed it). Pick it in the system's sound settings or in an app.
  Sound reaches the peer only while an app plays into it, and comes out of the peer's default
  output.
- **The speakers' owner decides.** Both sides are off by default: on the machine whose speakers
  should play, run `crosspanectl allow <peer> speaker` (`--off` to withdraw). Withdrawing it,
  locking the screen, `crosspanectl panic`, a lost connection or revoking the peer stops the sound
  within a second. After an unlock, an app has to start playing again.
- **`crosspanectl allow <peer> mic`** is accepted and stored, and answers that microphones are not
  supported yet: no microphone is ever opened. An app recording from "⟨peer⟩ microphone" hears
  silence, and the agent shows a refusal notice.
- **See it:** `crosspanectl status` shows, per peer, what it may do here (`speaker`, `mic`, ...) and
  whether it is playing on this machine's speakers; the tray / menu-bar menu lists the peers
  playing now, and its icon shows the active state.
- **Needs** PipeWire (Linux) or CoreAudio (Mac) at agent start; the agent logs "audio sharing on"
  (or why not) and then advertises the `audio` feature. `CROSSPANE_AUDIO=0` in the agent's
  environment turns audio off and never touches the audio server. Every script under
  `scripts/e2e/` that starts agents sets it (and `CROSSPANE_DISCOVERY=0`), except
  `audio-private.sh`, which runs on private audio servers.
- **Limits (v0):** the Mac resamples the 48 kHz stream to its default output's rate (44.1, 48,
  88.2, 96, 176.4 or 192 kHz; WP-3.7a/b); the Mac's driver serves one peer at a time;
  an agent whose audio worker died keeps audio refused until it restarts.
- **Check it without touching your audio:** `scripts/lead/impl-env.sh scripts/e2e/audio-private.sh`
  runs two agents, each on its own private PipeWire server, plays a 1 kHz tone into one's "speakers"
  and records what comes out of the other's output.

## Clipboard sharing (v0)

Clipboard sharing is off by default in both directions. In Settings, each paired machine has
two switches, enabled only when it advertises `clip/0`:

- `clipboard.read`: the peer may read **my** clipboard when I paste there.
- `clipboard.write`: the peer may offer **its** clipboard here, ready for a local paste.

Use `crosspanectl allow <peer> clipboard.read` or `clipboard.write`; add `--off` to withdraw.
Changes take effect when the next status confirms them. Copying advertises an offer without
reading content; bytes are read lazily for a paste. Text is limited to 1 MiB and images to 16 MiB.
Locking or an unknown session state blocks clipboard operations and retires promises; unlocking
does not restore an old promise. Clipboard contents are never logged.

Settings and `crosspanectl status` show node counters since agent startup: offers issued/observed, fetches
issued, and data responses issued (not delivery acknowledgements). Failure totals count typed
incoming/outgoing failures and mapped local promise, withdrawal and queue failures; these count
classified events, so one failed paste can contribute more than once. Empty answers
without a classified reason, including engine cancellation/expiry, are excluded. No content or
per-item sizes are shown. On macOS, if remote paste is blocked, check whether Crosspane's
**Paste from Other Apps** setting is Ask or AlwaysDeny; Settings does not read or change it.

## Known limitations (v0)

On Linux, lock-state sync writes only Crosspane's injected keyboard source. It does not directly
reset or set the physical keyboard or IME keyboard. An active IME may forward the source's
modifiers, so equality of lock states across the whole node is not verified. The physical keyboard
keeps its own lock state after control ends.

Drag a window across: on paired machines with drag enabled, drag its title bar against the
screen edge toward the peer. On Hyprland, release when the HUD says “Release to move”; the window
is projected there and ordinary pointer crossing resumes. Both peers must advertise `drag/0`;
`[drag] across = false` in `config.toml` disables the offer. `crosspanectl status` shows the
negotiated drag setting per peer and the active gesture's HUD label.
`[drag] push_to_cross_ms = 150` sets the edge dwell in milliseconds, clamped to 0–2000.

While controlling a peer, dragging a window back across the edge toward this machine uses
`drag/1`, negotiated by both peers under the same `[drag] across` switch. With a current native
move report from the peer, the window is dropped under the pointer here; dragging a proxy of
this machine's window back restores the original here. The drag never continues on this
machine: the routed primary-button release precedes the drop, and the later physical release
is swallowed. Esc cancels the current offer; a later native move report can re-arm it. Without
a current report or the required grants, the button-held crossing remains blocked or the
projection is refused. This requires a native-move detector on the peer; negotiated features
alone do not prove that its backend can report the gesture.

`crosspanectl status` shows `drag:on+in` when both `drag/0` and `drag/1` are currently negotiated
on the connected link, `drag:on` for `drag/0` only, `drag:in` for `drag/1` only, and `drag:off`
when neither is available. Old agent responses without `drag_in` retain their previous display.
The JSON peer fields keep the existing `drag` value and add the bilateral `drag_in` boolean.

- **Notifications** that your notification daemon shows on the output where a projected window is
  parked appear in that projected window (they're captured with it). Pinning the daemon to a real
  output (e.g. mako's `output=`) avoids it.
- **Cursor shape:** over a projected window the cursor takes the source app's shape (text beam,
  hand, resize arrows) when the source can see it: on a Mac source (needs Screen Recording).
  Windows from Hyprland show the destination's default arrow: Hyprland 0.56.2's cursor capture
  crashed the compositor once (02 §3.3), so it is off. `CROSSPANE_HYPR_CURSORS=1` in the agent's
  environment turns it on, and even then it only covers apps that draw their own cursor (GTK3,
  Firefox, Xwayland), not GTK4, Qt 6 or Chromium.
- **GPU paths** (docs/wp/GPU-v0.md) are on by default. On Hyprland, captures go straight into GPU
  memory (DMA-BUF) and NVENC encodes from it; on the Mac, captured frames are hashed on the GPU and
  VideoToolbox encodes and decodes without CPU copies. `CROSSPANE_GPU=0` in the agent's environment
  turns them off (CPU paths, as before); the agent logs "GPU frame capture on (DMA-BUF)" on Linux.
- **Region video:** when only part of a projected window moves, only that rectangle goes as video
  and the rest stays lossless. `CROSSPANE_REGION_VIDEO=0` on the *destination* keeps sources on
  whole-window video.
- **If Hyprland restarts** (e.g. after a crash), the agent notices that its instance is gone,
  exits and is started again by systemd on the new one.
- **Picking windows:** a peer's windows must be allowed once (`crosspanectl allow <peer> browse` on
  that peer). Window titles from a Mac need Screen Recording there.
- **Video:** H.264 needs a build with `--features video` (FFmpeg 9 on Linux; built in on the Mac);
  without it everything is lossless tiles, which is fine on a wired LAN but heavy for full-screen
  video over Wi-Fi.
