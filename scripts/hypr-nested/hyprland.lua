-- Standalone test configuration: never load the owner's config or Omarchy.
local width = os.getenv("CROSSPANE_NESTED_WIDTH") or "1280"
local height = os.getenv("CROSSPANE_NESTED_HEIGHT") or "720"

hl.monitor({ output = "", mode = width .. "x" .. height .. "@60", position = "auto", scale = 1 })
hl.config({
    animations = { enabled = false },
    decoration = { blur = { enabled = false }, shadow = { enabled = false } },
    misc = {
        disable_hyprland_logo = true,
        disable_splash_rendering = true,
        disable_watchdog_warning = true,
    },
    xwayland = { enabled = false },
})

-- Close a windowed nest by hand without any external command or IPC.
hl.bind("CTRL + ALT + Escape", hl.dsp.exit(), { description = "Exit Crosspane test session" })
