# Nested Hyprland test session

`scripts/hypr-nested.sh` runs a second Hyprland instance inside the owner's live session, so
automated tests and spikes never touch the live instance (AGENTS.md).

```sh
scripts/hypr-nested.sh start            # prints the instance signature and Wayland socket
eval "$(scripts/hypr-nested.sh env)"    # point this shell (hyprctl, Wayland clients) at it
scripts/hypr-nested.sh status
scripts/hypr-nested.sh stop
```

- `--name N` runs several instances side by side; `--width`/`--height` set the nested monitor mode.
- Headless outputs for parking tests are created inside the nested instance:
  `hyprctl output create headless <name>` after `eval "$(… env)"`.
- Hyprland 0.56 has no headless-only start. The nested instance is a window of class `aquamarine`
  on the live desktop, which the live session tiles like any other window.

**Optional, owner:** to keep it off the visible workspaces, add this rule to the live config (for
example `~/.config/hypr/hyprland.lua`):

```lua
o.window("^aquamarine$", { workspace = "special:crosspane-test silent" })
```
