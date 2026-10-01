//! M2 twin-output parking on Hyprland (03 §4.3, docs/wp/E2-v0.md decision 4; WP-2.7b, lead).
//!
//! A projected window moves alone onto a workspace of its own headless output. The output's mode
//! is the destination proxy's content size at the destination's scale, so the window renders at the
//! destination's density and is never visible on this node's screens. The output sits far from the
//! real monitors, so the physical pointer can't wander onto it.
//!
//! **No window is lost (04 §8 invariant 4):** every park is journaled (written and synced) before
//! anything changes. `recover()` undoes whatever a previous run left behind and removes leftover
//! outputs, by name prefix, even ones missing from the journal.
//!
//! **What "the window's size" means here (WP-2.35).** `hyprctl clients` reports Hyprland's *layout
//! goal* for a window (`GEOMETRIC_GOAL` in HyprCtl.cpp, v0.56.1): the geometry the compositor has
//! laid out and configured the client with, not the buffer the client has committed. Settling on
//! that goal is the best signal IPC offers. It shows that the compositor has finished tiling the
//! window onto the new mode and that the layout is stable; it cannot show that the client has
//! redrawn at that size. A client that ignores its configure is invisible to this module.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crosspane_platform::{Parked, ParkingKind, PlatformError, WindowParking};
use crosspane_types::geom::euclid::point2;
use crosspane_types::geom::{PixelRect, PixelSize};
use crosspane_types::id::{DisplayId, WindowId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ipc::HyprIpc;

/// Every output this module creates is named with this prefix.
pub const OUTPUT_PREFIX: &str = "CROSSPANE-";
/// Every parking workspace is named with this prefix.
pub const WORKSPACE_PREFIX: &str = "crosspane-";
/// Twin outputs start this far right of the origin, 16384 px apart, out of the physical pointer's
/// reach.
const PARK_ORIGIN_X: i64 = 1 << 20;
const PARK_STRIDE: i64 = 1 << 14;
/// How long to wait for Hyprland to apply a mode or move a window.
const SETTLE: Duration = Duration::from_millis(1500);
/// How long an app-chosen size must hold still before it counts as the size the app took.
const STABLE: Duration = Duration::from_millis(200);
/// How far, per dimension in device pixels, a window's content may sit from the requested mode
/// size and still count as filling it.
const FILL_TOLERANCE: u32 = 2;
/// Mode changes one settle may make for a bar that arrived late on the twin output.
const MAX_REPADS: u32 = 2;
/// Mode changes one settle may make to fit an app-chosen size that overflows the twin output (an
/// app with a minimum size larger than the request).
const MAX_FITS: u32 = 1;
/// After a twin output failed to come up, how long parks fail fast (`Unsupported`, so the window
/// is mirrored) instead of trying again: each try adds and removes an output, which re-tiles the
/// session.
const TWIN_RETRY: Duration = Duration::from_secs(600);

/// Where a window was before it was parked.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Original {
    workspace: String,
    floating: bool,
    at: [i64; 2],
    size: [i64; 2],
    fullscreen: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Entry {
    window: u64,
    address: String,
    output: String,
    workspace: String,
    slot: i64,
    original: Original,
}

#[derive(Debug)]
pub struct HyprlandParking {
    ipc: HyprIpc,
    journal: PathBuf,
    entries: BTreeMap<u64, Entry>,
    /// Per parked window, the largest area bars have reserved on its twin output (left, top,
    /// right, bottom; logical pixels). It only grows: a bar re-creates its surface after every
    /// mode change and is briefly gone, and shrinking the mode then makes it re-create the surface
    /// again. On 2026-10-01 that loop changed one twin's mode hundreds of times and Hyprland
    /// 0.56.2 crashed.
    padding: BTreeMap<u64, [u32; 4]>,
    /// When a twin output last failed to come up (see [`TWIN_RETRY`]).
    twin_failed: Option<Instant>,
}

impl HyprlandParking {
    /// Load the journal at `journal` (a missing file is empty). Call [`WindowParking::recover`]
    /// before parking anything.
    pub fn new(ipc: HyprIpc, journal: PathBuf) -> Result<HyprlandParking, PlatformError> {
        let entries = match std::fs::read_to_string(&journal) {
            Ok(text) => serde_json::from_str::<Vec<Entry>>(&text)
                .map_err(|e| backend(format!("parking journal {}: {e}", journal.display())))?
                .into_iter()
                .map(|e| (e.window, e))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(backend(format!("read {}: {e}", journal.display()))),
        };
        Ok(HyprlandParking {
            ipc,
            journal,
            entries,
            padding: BTreeMap::new(),
            twin_failed: None,
        })
    }

    /// Write the journal atomically and sync it before returning.
    fn save(&self) -> Result<(), PlatformError> {
        let entries: Vec<&Entry> = self.entries.values().collect();
        let text = serde_json::to_vec_pretty(&entries).map_err(|e| backend(e.to_string()))?;
        let tmp = self.journal.with_extension("tmp");
        let mut file = std::fs::File::create(&tmp).map_err(|e| backend(format!("journal: {e}")))?;
        file.write_all(&text)
            .map_err(|e| backend(format!("journal: {e}")))?;
        file.sync_all()
            .map_err(|e| backend(format!("journal: {e}")))?;
        std::fs::rename(&tmp, &self.journal).map_err(|e| backend(format!("journal: {e}")))?;
        if let Some(dir) = self.journal.parent()
            && let Ok(dir) = std::fs::File::open(dir)
        {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    fn client(&self, window: WindowId) -> Result<Option<Value>, PlatformError> {
        let clients = self.ipc.json("clients")?;
        Ok(clients.as_array().and_then(|list| {
            list.iter()
                .find(|c| stable_id(c) == Some(window.0))
                .cloned()
        }))
    }

    fn monitor(&self, name: &str) -> Result<Option<Value>, PlatformError> {
        let monitors = self.ipc.json("monitors")?;
        Ok(monitors.as_array().and_then(|list| {
            list.iter()
                .find(|m| m.get("name").and_then(Value::as_str) == Some(name))
                .cloned()
        }))
    }

    fn next_slot(&self) -> i64 {
        (0..)
            .find(|slot| !self.entries.values().any(|e| e.slot == *slot))
            .unwrap_or(0)
    }

    /// Set the twin output's mode and wait until Hyprland reports it.
    fn set_mode(
        &mut self,
        entry: &Entry,
        size: PixelSize,
        scale: f64,
    ) -> Result<(), PlatformError> {
        let reserved = self.reserved(&entry.output)?;
        let padding = self.grow_padding(entry.window, reserved);
        self.set_mode_padded(entry, size, scale, padding)
    }

    /// Grow the padding remembered for `window` to cover `reserved`, and return it.
    fn grow_padding(&mut self, window: u64, reserved: [u32; 4]) -> [u32; 4] {
        let padding = self.padding.entry(window).or_default();
        *padding = covering(*padding, reserved);
        *padding
    }

    /// The area other clients reserve on `output` (bars' exclusive zones), in logical pixels:
    /// left, top, right, bottom.
    fn reserved(&self, output: &str) -> Result<[u32; 4], PlatformError> {
        let mut r = [0; 4];
        if let Some(m) = self.monitor(output)?
            && let Some(a) = m.get("reserved").and_then(Value::as_array)
        {
            for (i, v) in a.iter().take(4).enumerate() {
                r[i] = v.as_u64().and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
            }
        }
        Ok(r)
    }

    /// Set the twin's mode so that, after the area bars reserve on it (Waybar puts one on every
    /// output), the work area is exactly `size`: the window tiles into that work area and the
    /// capture crops to the window, so the bar never shows in the projection.
    fn set_mode_padded(
        &self,
        entry: &Entry,
        size: PixelSize,
        scale: f64,
        reserved: [u32; 4],
    ) -> Result<(), PlatformError> {
        let scale = sane_scale(scale);
        let (w, h) = padded_mode(size, scale, reserved);
        let x = PARK_ORIGIN_X + entry.slot * PARK_STRIDE;
        // Every mode change reconfigures the output and makes bars re-create their surfaces;
        // don't make one that changes nothing.
        if let Some(m) = self.monitor(&entry.output)?
            && has_mode(&m, (w, h), scale, x)
        {
            return Ok(());
        }
        self.ipc.eval(&format!(
            "hl.monitor({{ output = \"{}\", mode = \"{w}x{h}@60\", position = \"{x}x0\", scale = {scale} }})",
            entry.output
        ))?;
        let deadline = Instant::now() + SETTLE;
        loop {
            if let Some(m) = self.monitor(&entry.output)? {
                let mw = m.get("width").and_then(Value::as_u64).unwrap_or(0);
                let mh = m.get("height").and_then(Value::as_u64).unwrap_or(0);
                if mw == u64::from(w) && mh == u64::from(h) {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err(PlatformError::Timeout);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn move_window(&self, address: &str, workspace: &str) -> Result<(), PlatformError> {
        self.ipc.dispatch(&format!(
            "hl.dsp.window.move({{ window = \"address:{address}\", workspace = \"{}\" }})",
            lua_escape(workspace)
        ))
    }

    fn undo(&mut self, window: u64) -> Result<(), PlatformError> {
        let Some(entry) = self.entries.get(&window).cloned() else {
            return Ok(());
        };
        if let Some(client) = self.client(WindowId(window))? {
            let address = client
                .get("address")
                .and_then(Value::as_str)
                .unwrap_or(&entry.address)
                .to_owned();
            // A park that failed before the window moved left it where it was: moving it "back"
            // would re-tile it.
            let on_twin = client.pointer("/workspace/name").and_then(Value::as_str)
                == Some(entry.workspace.as_str());
            let fullscreen = client
                .get("fullscreen")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let floating = client
                .get("floating")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let o = &entry.original;
            let changed = on_twin || fullscreen != o.fullscreen || floating != o.floating;
            if changed {
                self.ipc.dispatch(&format!(
                    "hl.dsp.window.fullscreen_state({{ window = \"address:{address}\", internal = 0, client = 0 }})"
                ))?;
            }
            if on_twin {
                self.move_window(&address, &workspace_selector(&o.workspace))?;
            }
            if changed && o.fullscreen != 0 {
                self.ipc.dispatch(&format!(
                    "hl.dsp.window.fullscreen_state({{ window = \"address:{address}\", internal = {0}, client = {0} }})",
                    o.fullscreen
                ))?;
            }
            if changed && o.floating {
                self.ipc.dispatch(&format!(
                    "hl.dsp.window.float({{ window = \"address:{address}\", action = \"set\" }})"
                ))?;
                self.ipc.dispatch(&format!(
                    "hl.dsp.window.resize({{ window = \"address:{address}\", x = {}, y = {}, relative = false }})",
                    o.size[0], o.size[1]
                ))?;
                self.ipc.dispatch(&format!(
                    "hl.dsp.window.move({{ window = \"address:{address}\", x = {}, y = {}, relative = false }})",
                    o.at[0], o.at[1]
                ))?;
            }
        }
        if self.monitor(&entry.output)?.is_some() {
            expect_ok(
                &self
                    .ipc
                    .request(&format!("output remove {}", entry.output))?,
            )?;
        }
        self.entries.remove(&window);
        self.padding.remove(&window);
        self.save()
    }
}

impl WindowParking for HyprlandParking {
    fn park(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        if self.entries.contains_key(&window.0) {
            return self.resize(window, size, scale);
        }
        if self
            .twin_failed
            .is_some_and(|failed| failed.elapsed() < TWIN_RETRY)
        {
            return Err(PlatformError::Unsupported("twin output unavailable"));
        }
        let client = self.client(window)?.ok_or(PlatformError::NotFound)?;
        let address = client
            .get("address")
            .and_then(Value::as_str)
            .ok_or_else(|| backend("client without an address".into()))?
            .to_owned();
        let pair = |key: &str| -> [i64; 2] {
            let v = client.get(key).and_then(Value::as_array);
            let at = |i: usize| {
                v.and_then(|a| a.get(i))
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
            };
            [at(0), at(1)]
        };
        let entry = Entry {
            window: window.0,
            address: address.clone(),
            output: format!("{OUTPUT_PREFIX}{:x}", window.0),
            workspace: format!("{WORKSPACE_PREFIX}{:x}", window.0),
            slot: self.next_slot(),
            original: Original {
                workspace: client
                    .pointer("/workspace/name")
                    .and_then(Value::as_str)
                    .unwrap_or("1")
                    .to_owned(),
                floating: client
                    .get("floating")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                at: pair("at"),
                size: pair("size"),
                fullscreen: client
                    .get("fullscreen")
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
            },
        };
        // What the window measured before it was parked, in the pixels it would have on the twin:
        // a client that still reports it has not answered the new geometry (see `Settling`).
        let before = device_size(
            (entry.original.size[0] as f64, entry.original.size[1] as f64),
            scale,
        );
        // Journal first: from here on a crash is recoverable.
        self.entries.insert(window.0, entry.clone());
        self.save()?;
        let result = (|| {
            // The rule comes first, so the new output's first workspace is ours (no stray ones).
            self.ipc.eval(&format!(
                "hl.workspace_rule({{ workspace = \"name:{}\", monitor = \"{}\", default = true, gaps_in = 0, gaps_out = 0, border_size = 0, no_rounding = true, no_shadow = true, decorate = false }})",
                entry.workspace, entry.output
            ))?;
            // A twin that can't be brought up (a nested Hyprland can't allocate headless
            // outputs) means M2 is unavailable here: `Unsupported`, so the agent mirrors the
            // window instead (M1, the reported fallback).
            let twin = (|| {
                if self.monitor(&entry.output)?.is_none() {
                    expect_ok(
                        &self
                            .ipc
                            .request(&format!("output create headless {}", entry.output))?,
                    )?;
                }
                self.set_mode(&entry, size, scale)
            })();
            if let Err(error) = twin {
                tracing::warn!(%error, output = entry.output, "no twin output; mirroring for a while");
                self.twin_failed = Some(Instant::now());
                return Err(PlatformError::Unsupported("twin output unavailable"));
            }
            if entry.original.fullscreen != 0 {
                self.ipc.dispatch(&format!(
                    "hl.dsp.window.fullscreen({{ window = \"address:{address}\", action = \"unset\" }})"
                ))?;
            }
            if entry.original.floating {
                self.ipc.dispatch(&format!(
                    "hl.dsp.window.float({{ window = \"address:{address}\", action = \"unset\" }})"
                ))?;
            }
            self.move_window(&address, &format!("name:{}", entry.workspace))?;
            self.settle(window, size, scale, before)
        })();
        if result.is_err() {
            // Don't leave a half-parked window behind.
            if let Err(e) = self.undo(window.0) {
                tracing::warn!(error = %e, "undoing a failed park failed; recover() will retry");
            }
        }
        result
    }

    fn resize(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        let entry = self
            .entries
            .get(&window.0)
            .cloned()
            .ok_or(PlatformError::NotFound)?;
        // The size the client has now, taken before the mode changes under it.
        let client = self.client(window)?.ok_or(PlatformError::NotFound)?;
        let before = client_size(&client).and_then(|logical| device_size(logical, scale));
        self.set_mode(&entry, size, scale)?;
        self.settle(window, size, scale, before)
    }

    fn geometry(&self, window: WindowId) -> Result<Parked, PlatformError> {
        let entry = self.entries.get(&window.0).ok_or(PlatformError::NotFound)?;
        let client = self.client(window)?.ok_or(PlatformError::NotFound)?;
        let monitor = self
            .monitor(&entry.output)?
            .ok_or(PlatformError::NotFound)?;
        clipped(
            parked_from(window, &client, &monitor)?,
            output_extent(&monitor)?,
        )
    }

    fn restore(&mut self, window: WindowId) -> Result<(), PlatformError> {
        self.undo(window.0)
    }

    fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
        let windows: Vec<u64> = self.entries.keys().copied().collect();
        let mut restored = Vec::new();
        for window in windows {
            match self.undo(window) {
                Ok(()) => restored.push(WindowId(window)),
                Err(e) => tracing::warn!(error = %e, window, "could not restore a parked window"),
            }
        }
        // Outputs left by a run whose journal write didn't happen (or was lost).
        if let Some(monitors) = self.ipc.json("monitors")?.as_array() {
            for name in monitors
                .iter()
                .filter_map(|m| m.get("name").and_then(Value::as_str))
                .filter(|n| n.starts_with(OUTPUT_PREFIX))
            {
                let _ = self.ipc.request(&format!("output remove {name}"));
            }
        }
        if self.entries.is_empty() {
            Ok(restored)
        } else {
            Err(backend(format!(
                "{} parked windows could not be restored",
                self.entries.len()
            )))
        }
    }
}

impl HyprlandParking {
    /// Wait until the window has taken its new geometry on its twin output, then report it.
    ///
    /// `before` is the window's content size before the mode changed, in device pixels at the new
    /// scale (`None` when unknown). The decision is [`Settling`]'s; this loop polls, and makes the
    /// mode changes the decision asks for: repads for a late bar, and one fit for an app that
    /// chose a size larger than the output.
    ///
    /// The size polled is `hyprctl`'s layout goal for the window, not its committed buffer (see
    /// the module docs): settling means the compositor has laid the window out on the new mode and
    /// the layout holds still. A client that ignores its configure can't be seen from here.
    fn settle(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
        before: Option<(i32, i32)>,
    ) -> Result<Parked, PlatformError> {
        let entry = self
            .entries
            .get(&window.0)
            .cloned()
            .ok_or(PlatformError::NotFound)?;
        let (w, h) = mode_size(size, sane_scale(scale));
        let requested = (
            i32::try_from(w).unwrap_or(i32::MAX),
            i32::try_from(h).unwrap_or(i32::MAX),
        );
        let started = Instant::now();
        let mut settling = Settling::new(requested, before);
        // The work-area size the twin is currently set up for: `size`, until a fit grows it.
        let mut want = size;
        let (mut repads, mut fits) = (0, 0);
        loop {
            // A bar can arrive on the new output after its mode was set: pad for it, a bounded
            // number of times. A bar that is (briefly) gone never shrinks the padding.
            let reserved = self.reserved(&entry.output)?;
            let padding = self.padding.get(&window.0).copied().unwrap_or_default();
            if repads < MAX_REPADS && covering(padding, reserved) != padding {
                repads += 1;
                // The mode is about to change again. What the window showed before it is no
                // evidence about what it shows after: take its size now as the baseline and start
                // the stability clock over.
                settling.restart(self.baseline(window, scale)?);
                let padding = self.grow_padding(window.0, reserved);
                self.set_mode_padded(&entry, want, scale, padding)?;
            }
            let client = self.client(window)?.ok_or(PlatformError::NotFound)?;
            let monitor = self
                .monitor(&entry.output)?
                .ok_or(PlatformError::NotFound)?;
            let parked = parked_from(window, &client, &monitor)?;
            let on_twin = client.pointer("/workspace/name").and_then(Value::as_str)
                == Some(entry.workspace.as_str());
            let elapsed = started.elapsed();
            let decision = settling.observe(Poll {
                content: parked.content,
                extent: output_extent(&monitor)?,
                on_twin,
                elapsed,
            });
            let log = |outcome: &str, content: Option<PixelRect>| {
                tracing::debug!(
                    requested = ?requested,
                    taken = ?content.map(|c| (c.width(), c.height())),
                    outcome,
                    repads,
                    fits,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "settled a twin window"
                );
            };
            match decision {
                Decision::Done { content, rule } => {
                    // An app may refuse a size; the geometry is what it took.
                    log(rule.as_str(), Some(content));
                    return Ok(Parked { content, ..parked });
                }
                Decision::Timeout => {
                    log("timeout", None);
                    return Err(PlatformError::Timeout);
                }
                Decision::Grow { size: fit } => {
                    // The app chose a size the output can't hold (a minimum size): grow the
                    // output to it, keeping the bars' padding. `Settling` has already taken the
                    // size as its baseline and its new target.
                    fits += 1;
                    want = PixelSize::new(
                        u32::try_from(fit.0).unwrap_or(0),
                        u32::try_from(fit.1).unwrap_or(0),
                    );
                    let padding = self.padding.get(&window.0).copied().unwrap_or_default();
                    self.set_mode_padded(&entry, want, scale, padding)?;
                }
                Decision::Wait => {}
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The window's content size now, in device pixels at `scale`: the baseline for deciding
    /// whether the client has answered a mode change about to be made.
    fn baseline(&self, window: WindowId, scale: f64) -> Result<Option<(i32, i32)>, PlatformError> {
        let client = self.client(window)?.ok_or(PlatformError::NotFound)?;
        Ok(client_size(&client).and_then(|logical| device_size(logical, scale)))
    }
}

/// One poll of a settling window.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Poll {
    /// The window's content in the twin output's device pixels.
    content: PixelRect,
    /// The twin output's current mode, from its origin, in the same pixels.
    extent: PixelRect,
    /// Whether the window is on its twin workspace.
    on_twin: bool,
    /// Time since the settle began.
    elapsed: Duration,
}

/// Which rule completed a settle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rule {
    /// The content filled the requested size.
    Exact,
    /// The app took a size of its own and held it.
    Stable,
    /// [`SETTLE`] ran out with the window on its twin.
    Deadline,
}

impl Rule {
    fn as_str(self) -> &'static str {
        match self {
            Rule::Exact => "exact",
            Rule::Stable => "stable",
            Rule::Deadline => "deadline",
        }
    }
}

/// The answer to one poll.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Decision {
    Wait,
    /// Settled: the window's geometry (clipped to the output only at the deadline).
    Done {
        content: PixelRect,
        rule: Rule,
    },
    /// The app chose a size the output can't hold: grow the output's work area to `size`, then
    /// keep polling. [`Settling`] has already taken `size` as its new target.
    Grow {
        size: (i32, i32),
    },
    Timeout,
}

/// Decides when a window on a twin output has taken the size it was asked for, from one poll at a
/// time. It reads no clock and makes no request: time is a [`Poll`] input, so tests drive it.
///
/// The output's mode and the client's size change at different moments, and the output can change
/// first. After a shrink the old, larger window then still reports its old size; that is *never* a
/// completion (it was accepted as one before WP-2.35). A poll is *present* while the window is on
/// its twin workspace with non-empty content. Then:
///
/// 1. **Exact:** the content lies inside the output's extent and is within [`FILL_TOLERANCE`] of
///    the target size in both directions of both dimensions.
/// 2. **Stable:** the content lies inside the extent, has kept one size for [`STABLE`], and that
///    size differs from the size before the mode changed (so the client answered). This covers
///    apps with a minimum, maximum or fixed-aspect size that fits the output.
/// 3. **Grow:** the same evidence as rule 2, but the content overflows the extent: an app with a
///    minimum size larger than the request. The answer is to grow the output to the app's size
///    (once, [`MAX_FITS`]): the target becomes that size, the size becomes the new baseline and
///    the stability clock restarts, so normally Exact completes on the grown output with the
///    app's own, unclipped size. Only while time remains: a fit asks for a mode change, which can
///    take as long as a whole settle to show, so at the deadline (or past it) rule 4 applies
///    instead.
/// 4. **Deadline:** at [`SETTLE`] a window on its twin completes with its content clipped to the
///    output (a fixed-size app larger than the output, or one that never settled); one that is
///    not on its twin is a timeout.
///
/// The caller calls [`Settling::restart`] before every mode change it makes itself.
#[derive(Debug)]
struct Settling {
    /// The work-area size the window is expected to fill.
    requested: (i32, i32),
    /// The window's size before the latest mode change: a client still showing it has not answered.
    before: Option<(i32, i32)>,
    /// The size the window has held, and the elapsed time of the first poll that saw it.
    steady: Option<((i32, i32), Duration)>,
    /// Fits still allowed.
    fits_left: u32,
}

impl Settling {
    fn new(requested: (i32, i32), before: Option<(i32, i32)>) -> Settling {
        Settling {
            requested,
            before,
            steady: None,
            fits_left: MAX_FITS,
        }
    }

    /// The caller is about to change the mode again: `before` (the window's size now, if known)
    /// is the new baseline, and nothing seen so far counts as evidence about the new mode.
    fn restart(&mut self, before: Option<(i32, i32)>) {
        self.before = before;
        self.steady = None;
    }

    fn observe(&mut self, poll: Poll) -> Decision {
        let size = (poll.content.width(), poll.content.height());
        let present = poll.on_twin && size.0 > 0 && size.1 > 0;
        self.steady = match self.steady {
            _ if !present => None,
            Some((held, since)) if held == size => Some((held, since)),
            _ => Some((size, poll.elapsed)),
        };
        if present {
            let answered = self.before.is_some_and(|before| before != size);
            let held = self
                .steady
                .is_some_and(|(_, since)| poll.elapsed.saturating_sub(since) >= STABLE);
            if inside(&poll.extent, &poll.content) {
                let fills = size.0.abs_diff(self.requested.0) <= FILL_TOLERANCE
                    && size.1.abs_diff(self.requested.1) <= FILL_TOLERANCE;
                if fills {
                    return Decision::Done {
                        content: poll.content,
                        rule: Rule::Exact,
                    };
                }
                if answered && held {
                    return Decision::Done {
                        content: poll.content,
                        rule: Rule::Stable,
                    };
                }
            } else if answered && held && self.fits_left > 0 && poll.elapsed < SETTLE {
                // Only a size larger than the target can be cured by a larger output; content
                // that merely sits partly outside it (its size is within the target) cannot.
                let fit = (self.requested.0.max(size.0), self.requested.1.max(size.1));
                if fit != self.requested {
                    self.fits_left -= 1;
                    self.requested = fit;
                    self.restart(Some(size));
                    return Decision::Grow { size: fit };
                }
            }
        }
        if poll.elapsed < SETTLE {
            return Decision::Wait;
        }
        if !poll.on_twin {
            return Decision::Timeout;
        }
        // A window with nothing inside the output has no geometry to report.
        match poll.content.intersection(&poll.extent) {
            Some(content) => Decision::Done {
                content,
                rule: Rule::Deadline,
            },
            None => Decision::Timeout,
        }
    }
}

/// Whether `content` lies wholly inside `extent`.
fn inside(extent: &PixelRect, content: &PixelRect) -> bool {
    content.min.x >= extent.min.x
        && content.min.y >= extent.min.y
        && content.max.x <= extent.max.x
        && content.max.y <= extent.max.y
}

/// The twin output's current mode as a rectangle from its origin, in device pixels.
fn output_extent(monitor: &Value) -> Result<PixelRect, PlatformError> {
    let dim = |key: &str| {
        monitor
            .get(key)
            .and_then(Value::as_i64)
            .and_then(|v| i32::try_from(v).ok())
            .filter(|v| *v > 0)
    };
    match (dim("width"), dim("height")) {
        (Some(w), Some(h)) => Ok(PixelRect::new(point2(0, 0), point2(w, h))),
        _ => Err(backend("twin output without a mode".into())),
    }
}

/// `parked` with its content clipped to `extent`. A window with nothing inside the output has no
/// geometry to report.
fn clipped(parked: Parked, extent: PixelRect) -> Result<Parked, PlatformError> {
    let content = parked
        .content
        .intersection(&extent)
        .ok_or_else(|| backend("parked window lies outside its twin output".into()))?;
    Ok(Parked { content, ..parked })
}

/// A client's `size` (logical pixels), if it reports one.
fn client_size(client: &Value) -> Option<(f64, f64)> {
    let size = client.get("size")?.as_array()?;
    Some((size.first()?.as_f64()?, size.get(1)?.as_f64()?))
}

/// A logical size in device pixels at `scale`, as [`parked_from`] measures a window; `None` for an
/// empty one.
fn device_size((w, h): (f64, f64), scale: f64) -> Option<(i32, i32)> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let scale = sane_scale(scale);
    Some(((w * scale).round() as i32, (h * scale).round() as i32))
}

/// The parked window's content in device pixels of its twin output.
fn parked_from(window: WindowId, client: &Value, monitor: &Value) -> Result<Parked, PlatformError> {
    let num = |v: &Value, key: &str| v.get(key).and_then(Value::as_f64).unwrap_or(0.0);
    let pair = |key: &str| -> (f64, f64) {
        let a = client.get(key).and_then(Value::as_array);
        let at = |i: usize| {
            a.and_then(|a| a.get(i))
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
        };
        (at(0), at(1))
    };
    let scale = sane_scale(num(monitor, "scale"));
    let (mx, my) = (num(monitor, "x"), num(monitor, "y"));
    let (x, y) = pair("at");
    let (w, h) = pair("size");
    let to_px = |v: f64| (v * scale).round() as i32;
    let min = (to_px(x - mx), to_px(y - my));
    let id = monitor
        .get("id")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| backend("twin output without an id".into()))?;
    Ok(Parked {
        window,
        kind: ParkingKind::Twin,
        display: DisplayId(id),
        content: PixelRect::new(
            crosspane_types::geom::euclid::point2(min.0, min.1),
            crosspane_types::geom::euclid::point2(min.0 + to_px(w), min.1 + to_px(h)),
        ),
    })
}

fn stable_id(client: &Value) -> Option<u64> {
    client
        .get("stableId")
        .and_then(Value::as_str)
        .and_then(|s| u64::from_str_radix(s, 16).ok())
}

/// The smallest padding that covers both `a` and `b`, side by side.
fn covering(a: [u32; 4], b: [u32; 4]) -> [u32; 4] {
    [
        a[0].max(b[0]),
        a[1].max(b[1]),
        a[2].max(b[2]),
        a[3].max(b[3]),
    ]
}

/// Whether `monitor` (from `hyprctl monitors -j`) already has this mode, scale and position.
fn has_mode(monitor: &Value, (w, h): (u32, u32), scale: f64, x: i64) -> bool {
    let int = |key: &str| monitor.get(key).and_then(Value::as_i64);
    int("width") == Some(i64::from(w))
        && int("height") == Some(i64::from(h))
        && int("x") == Some(x)
        && int("y") == Some(0)
        && monitor
            .get("scale")
            .and_then(Value::as_f64)
            .is_some_and(|s| (s - scale).abs() < 1e-3)
}

/// A mode whose size is a whole number of logical pixels at `scale` (Hyprland rejects others).
/// `mode_size` plus the reserved area (logical pixels, converted to device pixels).
fn padded_mode(size: PixelSize, scale: f64, reserved: [u32; 4]) -> (u32, u32) {
    let (w, h) = mode_size(size, scale);
    let px = |v: u32| (f64::from(v) * scale).round() as u32;
    (
        w.saturating_add(px(reserved[0]))
            .saturating_add(px(reserved[2])),
        h.saturating_add(px(reserved[1]))
            .saturating_add(px(reserved[3])),
    )
}

fn mode_size(size: PixelSize, scale: f64) -> (u32, u32) {
    let fit = |v: u32| {
        let logical = (f64::from(v.max(64)) / scale).floor().max(32.0);
        (logical * scale).round() as u32
    };
    (fit(size.width), fit(size.height))
}

/// Integer scales only in v0 (the macOS destination is 1 or 2); anything else rounds.
fn sane_scale(scale: f64) -> f64 {
    if scale.is_finite() && scale >= 1.0 {
        scale.round().clamp(1.0, 4.0)
    } else {
        1.0
    }
}

fn workspace_selector(name: &str) -> String {
    if name.parse::<i64>().is_ok() || name.starts_with("special:") || name.starts_with("name:") {
        name.to_owned()
    } else {
        format!("name:{name}")
    }
}

fn lua_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn expect_ok(reply: &str) -> Result<(), PlatformError> {
    if reply.trim() == "ok" {
        Ok(())
    } else {
        Err(backend(format!("hyprland: {}", reply.trim())))
    }
}

fn backend(message: String) -> PlatformError {
    PlatformError::Backend(message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn mode_sizes_are_whole_logical_pixels() {
        assert_eq!(mode_size(PixelSize::new(1600, 1201), 2.0), (1600, 1200));
        assert_eq!(mode_size(PixelSize::new(800, 600), 1.0), (800, 600));
        assert_eq!(mode_size(PixelSize::new(1, 1), 2.0), (64, 64));
        assert_eq!(
            padded_mode(PixelSize::new(800, 600), 1.0, [0, 26, 0, 0]),
            (800, 626)
        );
        assert_eq!(
            padded_mode(PixelSize::new(1600, 1200), 2.0, [0, 26, 0, 10]),
            (1600, 1272)
        );
        assert_eq!(sane_scale(1.25), 1.0);
        assert_eq!(sane_scale(2.0), 2.0);
        assert_eq!(sane_scale(f64::NAN), 1.0);
    }

    #[test]
    fn padding_only_grows() {
        let dir = std::env::temp_dir().join(format!("cp-pad-{}", std::process::id()));
        let ipc = HyprIpc::new("none", &dir, Duration::from_millis(10));
        let mut p = HyprlandParking::new(ipc, dir.join("parking.json")).unwrap();
        assert_eq!(p.grow_padding(5, [0, 26, 0, 0]), [0, 26, 0, 0]);
        // The bar re-creating its surface: briefly nothing reserved. The padding stays.
        assert_eq!(p.grow_padding(5, [0, 0, 0, 0]), [0, 26, 0, 0]);
        assert_eq!(p.grow_padding(5, [0, 20, 0, 10]), [0, 26, 0, 10]);
        assert_eq!(p.grow_padding(6, [0, 0, 0, 0]), [0, 0, 0, 0]);
    }

    #[test]
    fn existing_modes_are_recognised() {
        let m =
            serde_json::json!({"width": 1072, "height": 990, "x": 1048576, "y": 0, "scale": 2.0});
        assert!(has_mode(&m, (1072, 990), 2.0, 1_048_576));
        assert!(!has_mode(&m, (1072, 938), 2.0, 1_048_576));
        assert!(!has_mode(&m, (1072, 990), 1.0, 1_048_576));
        assert!(!has_mode(&m, (1072, 990), 2.0, 1_064_960));
        assert!(!has_mode(
            &serde_json::json!({}),
            (1072, 990),
            2.0,
            1_048_576
        ));
    }

    #[test]
    fn workspace_selectors() {
        assert_eq!(workspace_selector("3"), "3");
        assert_eq!(workspace_selector("special:scratch"), "special:scratch");
        assert_eq!(workspace_selector("web"), "name:web");
    }

    #[test]
    fn content_in_twin_pixels() {
        let client = serde_json::json!({"at": [1048576, 0], "size": [800, 600]});
        let monitor = serde_json::json!({"id": 7, "x": 1048576, "y": 0, "scale": 2.0});
        let parked = parked_from(WindowId(1), &client, &monitor).unwrap();
        assert_eq!(parked.display, DisplayId(7));
        assert_eq!(
            parked.content,
            PixelRect::new(
                crosspane_types::geom::euclid::point2(0, 0),
                crosspane_types::geom::euclid::point2(1600, 1200)
            )
        );
    }

    fn rect(x0: i32, y0: i32, x1: i32, y1: i32) -> PixelRect {
        PixelRect::new(point2(x0, y0), point2(x1, y1))
    }

    /// Content of `w`×`h` at the output's origin.
    fn at_origin(w: i32, h: i32) -> PixelRect {
        rect(0, 0, w, h)
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn poll(content: PixelRect, extent: PixelRect, t: u64) -> Poll {
        Poll {
            content,
            extent,
            on_twin: true,
            elapsed: ms(t),
        }
    }

    fn done(content: PixelRect, rule: Rule) -> Decision {
        Decision::Done { content, rule }
    }

    #[test]
    fn shrink_ordering_never_accepts_the_old_window() {
        // 1600x1200 is requested down to 1200x900. The output shrinks first: the client still
        // reports its old size for three polls, then takes the new one.
        let extent = at_origin(1200, 900);
        let mut s = Settling::new((1200, 900), Some((1600, 1200)));
        for t in [0, 20, 40] {
            assert_eq!(
                s.observe(poll(at_origin(1600, 1200), extent, t)),
                Decision::Wait
            );
        }
        assert_eq!(
            s.observe(poll(at_origin(1200, 900), extent, 60)),
            done(at_origin(1200, 900), Rule::Exact)
        );
    }

    #[test]
    fn a_stale_window_never_completes_before_the_deadline() {
        // The old window holds its size far longer than STABLE: still not a completion, under
        // either rule, until the deadline clips it.
        let extent = at_origin(1200, 900);
        let mut s = Settling::new((1200, 900), Some((1600, 1200)));
        let mut t = 0;
        while t < 1500 {
            assert_eq!(
                s.observe(poll(at_origin(1600, 1200), extent, t)),
                Decision::Wait,
                "at {t} ms"
            );
            t += 20;
        }
        assert_eq!(
            s.observe(poll(at_origin(1600, 1200), extent, 1500)),
            done(at_origin(1200, 900), Rule::Deadline)
        );
    }

    #[test]
    fn grow_completes_once_the_client_fills() {
        let extent = at_origin(1600, 1200);
        let mut s = Settling::new((1600, 1200), Some((800, 600)));
        assert_eq!(
            s.observe(poll(at_origin(800, 600), extent, 0)),
            Decision::Wait
        );
        assert_eq!(
            s.observe(poll(at_origin(1100, 800), extent, 20)),
            Decision::Wait
        );
        assert_eq!(
            s.observe(poll(at_origin(1600, 1200), extent, 40)),
            done(at_origin(1600, 1200), Rule::Exact)
        );
    }

    #[test]
    fn grow_ordering_never_accepts_the_unchanged_client() {
        // The output grew first. The client's old size fits the new output and holds: it has not
        // answered (it equals the size before the mode change), so that is not an app-chosen size.
        let extent = at_origin(1600, 1200);
        let mut s = Settling::new((1600, 1200), Some((800, 600)));
        let mut t = 0;
        while t < 1500 {
            assert_eq!(
                s.observe(poll(at_origin(800, 600), extent, t)),
                Decision::Wait,
                "at {t} ms"
            );
            t += 20;
        }
        assert_eq!(
            s.observe(poll(at_origin(800, 600), extent, 1500)),
            done(at_origin(800, 600), Rule::Deadline)
        );
    }

    #[test]
    fn fill_tolerance_is_two_pixels_both_ways_in_both_dimensions() {
        // A padded output (bars) is a little larger than the request, so content slightly over
        // the request still lies inside it.
        let extent = at_origin(1210, 910);
        let case = |w: i32, h: i32| {
            Settling::new((1200, 900), None).observe(poll(at_origin(w, h), extent, 0))
        };
        for (w, h) in [
            (1200, 900),
            (1198, 898),
            (1202, 902),
            (1198, 902),
            (1202, 898),
        ] {
            assert_eq!(case(w, h), done(at_origin(w, h), Rule::Exact), "{w}x{h}");
        }
        for (w, h) in [
            (1197, 900),
            (1203, 900),
            (1200, 897),
            (1200, 903),
            (1203, 903),
        ] {
            assert_eq!(case(w, h), Decision::Wait, "{w}x{h}");
        }
    }

    #[test]
    fn minimum_size_completes_after_stable_not_before() {
        // 300x200 is requested; the app cannot go below 400x300 and holds that. The output has
        // room for it.
        let extent = at_origin(500, 400);
        let mut s = Settling::new((300, 200), Some((800, 600)));
        assert_eq!(
            s.observe(poll(at_origin(800, 600), extent, 0)),
            Decision::Wait
        );
        assert_eq!(
            s.observe(poll(at_origin(800, 600), extent, 20)),
            Decision::Wait
        );
        // The first poll that sees 400x300 starts the clock.
        assert_eq!(
            s.observe(poll(at_origin(400, 300), extent, 40)),
            Decision::Wait
        );
        let mut t = 60;
        while t < 40 + 200 {
            assert_eq!(
                s.observe(poll(at_origin(400, 300), extent, t)),
                Decision::Wait,
                "at {t} ms"
            );
            t += 20;
        }
        assert_eq!(
            s.observe(poll(at_origin(400, 300), extent, 240)),
            done(at_origin(400, 300), Rule::Stable)
        );
    }

    #[test]
    fn a_size_that_keeps_changing_never_counts_as_stable() {
        let extent = at_origin(1200, 900);
        let mut s = Settling::new((1200, 900), Some((1000, 800)));
        // An animation: a new size every poll for a second.
        for (i, t) in (0..1000).step_by(20).enumerate() {
            let w = 700 + i32::try_from(i).unwrap();
            assert_eq!(
                s.observe(poll(at_origin(w, 500), extent, t)),
                Decision::Wait,
                "at {t} ms"
            );
        }
        // It settles at 800x500: STABLE counts from the poll that first saw it.
        assert_eq!(
            s.observe(poll(at_origin(800, 500), extent, 1000)),
            Decision::Wait
        );
        assert_eq!(
            s.observe(poll(at_origin(800, 500), extent, 1180)),
            Decision::Wait
        );
        assert_eq!(
            s.observe(poll(at_origin(800, 500), extent, 1200)),
            done(at_origin(800, 500), Rule::Stable)
        );
    }

    #[test]
    fn leaving_the_twin_restarts_the_stable_clock() {
        let extent = at_origin(1200, 900);
        let mut s = Settling::new((1200, 900), Some((1000, 800)));
        assert_eq!(
            s.observe(poll(at_origin(800, 500), extent, 0)),
            Decision::Wait
        );
        let off = Poll {
            on_twin: false,
            ..poll(at_origin(800, 500), extent, 100)
        };
        assert_eq!(s.observe(off), Decision::Wait);
        // 200 ms after the first sighting, but the run was broken at 100 ms.
        assert_eq!(
            s.observe(poll(at_origin(800, 500), extent, 200)),
            Decision::Wait
        );
        assert_eq!(
            s.observe(poll(at_origin(800, 500), extent, 400)),
            done(at_origin(800, 500), Rule::Stable)
        );
    }

    #[test]
    fn without_a_size_from_before_only_an_exact_fill_or_the_deadline_completes() {
        let extent = at_origin(1200, 900);
        let mut s = Settling::new((1200, 900), None);
        let mut t = 0;
        while t < 1500 {
            assert_eq!(
                s.observe(poll(at_origin(1000, 700), extent, t)),
                Decision::Wait,
                "at {t} ms"
            );
            t += 100;
        }
        assert_eq!(
            s.observe(poll(at_origin(1000, 700), extent, 1500)),
            done(at_origin(1000, 700), Rule::Deadline)
        );
    }

    #[test]
    fn an_app_that_never_changes_completes_at_the_deadline_clipped() {
        // A fixed-size app, larger than the output it was given.
        let extent = at_origin(1200, 900);
        let mut s = Settling::new((1200, 900), Some((1600, 1200)));
        assert_eq!(
            s.observe(poll(rect(0, 0, 1600, 1200), extent, 1480)),
            Decision::Wait
        );
        // Its position matters too: only the part on the output is reported.
        assert_eq!(
            s.observe(poll(rect(100, 50, 1700, 1250), extent, 1500)),
            done(rect(100, 50, 1200, 900), Rule::Deadline)
        );
    }

    #[test]
    fn a_window_off_its_twin_times_out_at_the_deadline() {
        let extent = at_origin(1200, 900);
        let mut s = Settling::new((1200, 900), Some((1600, 1200)));
        let off = |t: u64| Poll {
            on_twin: false,
            // Exactly the size asked for does not matter while it is somewhere else.
            ..poll(at_origin(1200, 900), extent, t)
        };
        assert_eq!(s.observe(off(0)), Decision::Wait);
        assert_eq!(s.observe(off(1000)), Decision::Wait);
        assert_eq!(s.observe(off(1480)), Decision::Wait);
        assert_eq!(s.observe(off(1500)), Decision::Timeout);
    }

    #[test]
    fn content_outside_the_output_never_completes_early() {
        // The exact size, but hanging off the output's right edge.
        let extent = at_origin(1200, 900);
        let mut s = Settling::new((1200, 900), Some((1600, 1200)));
        let mut t = 0;
        while t < 1500 {
            assert_eq!(
                s.observe(poll(rect(100, 0, 1300, 900), extent, t)),
                Decision::Wait,
                "at {t} ms"
            );
            t += 20;
        }
        assert_eq!(
            s.observe(poll(rect(100, 0, 1300, 900), extent, 1500)),
            done(rect(100, 0, 1200, 900), Rule::Deadline)
        );
        // Starting above or left of the output is outside too.
        let mut s = Settling::new((1200, 900), Some((1600, 1200)));
        assert_eq!(
            s.observe(poll(rect(-5, 0, 1195, 900), extent, 0)),
            Decision::Wait
        );
        assert_eq!(
            s.observe(poll(rect(0, -5, 1200, 895), extent, 20)),
            Decision::Wait
        );
    }

    #[test]
    fn a_minimum_size_larger_than_the_request_grows_the_output_and_keeps_its_size() {
        // The real case: 300x200 is requested from 800x600, so the output's extent is the 300x200
        // mode itself. The app holds 400x300, which the output can't hold.
        let small = at_origin(300, 200);
        let mut s = Settling::new((300, 200), Some((800, 600)));
        assert_eq!(
            s.observe(poll(at_origin(800, 600), small, 0)),
            Decision::Wait
        );
        assert_eq!(
            s.observe(poll(at_origin(800, 600), small, 20)),
            Decision::Wait
        );
        assert_eq!(
            s.observe(poll(at_origin(400, 300), small, 40)),
            Decision::Wait
        );
        let mut t = 60;
        while t < 240 {
            assert_eq!(
                s.observe(poll(at_origin(400, 300), small, t)),
                Decision::Wait,
                "at {t} ms"
            );
            t += 20;
        }
        // Held for STABLE and different from before: the output grows to the app's size.
        assert_eq!(
            s.observe(poll(at_origin(400, 300), small, 240)),
            Decision::Grow { size: (400, 300) }
        );
        // The output now holds it: the app's own size, unclipped.
        let grown = at_origin(400, 300);
        assert_eq!(
            s.observe(poll(at_origin(400, 300), grown, 260)),
            done(at_origin(400, 300), Rule::Exact)
        );
    }

    #[test]
    fn the_fit_comes_exactly_when_the_size_has_held_for_stable() {
        let small = at_origin(300, 200);
        let grown = at_origin(400, 300);
        let mut s = Settling::new((300, 200), Some((800, 600)));
        assert_eq!(
            s.observe(poll(at_origin(400, 300), small, 0)),
            Decision::Wait
        );
        assert_eq!(
            s.observe(poll(at_origin(400, 300), small, 199)),
            Decision::Wait
        );
        assert_eq!(
            s.observe(poll(at_origin(400, 300), small, 200)),
            Decision::Grow { size: (400, 300) }
        );
        // After the fit the app's size is the baseline: a client that still shows it is not
        // "answering" anything, but it fills the grown target.
        assert_eq!(
            s.observe(poll(at_origin(400, 300), grown, 220)),
            done(at_origin(400, 300), Rule::Exact)
        );
    }

    #[test]
    fn only_one_fit_is_made_and_the_deadline_clips_what_still_overflows() {
        let small = at_origin(300, 200);
        let grown = at_origin(400, 300);
        let mut s = Settling::new((300, 200), Some((800, 600)));
        assert_eq!(
            s.observe(poll(at_origin(400, 300), small, 0)),
            Decision::Wait
        );
        assert_eq!(
            s.observe(poll(at_origin(400, 300), small, 200)),
            Decision::Grow { size: (400, 300) }
        );
        // The app keeps growing past the grown output and holds 500x400: no second fit.
        let mut t = 220;
        while t < 1500 {
            assert_eq!(
                s.observe(poll(at_origin(500, 400), grown, t)),
                Decision::Wait,
                "at {t} ms"
            );
            t += 20;
        }
        assert_eq!(
            s.observe(poll(at_origin(500, 400), grown, 1500)),
            done(at_origin(400, 300), Rule::Deadline)
        );
    }

    #[test]
    fn a_fit_is_never_started_at_the_deadline_or_after_it() {
        let small = at_origin(300, 200);
        // 400x300 is first seen at 1300 ms, so it has held for STABLE exactly at 1500 ms: the
        // deadline poll. The deadline rule applies (clipped), not a fit.
        let mut s = Settling::new((300, 200), Some((800, 600)));
        let mut t = 0;
        while t < 1300 {
            assert_eq!(
                s.observe(poll(at_origin(800, 600), small, t)),
                Decision::Wait,
                "at {t} ms"
            );
            t += 20;
        }
        for t in [1300, 1400, 1499] {
            assert_eq!(
                s.observe(poll(at_origin(400, 300), small, t)),
                Decision::Wait,
                "at {t} ms"
            );
        }
        assert_eq!(
            s.observe(poll(at_origin(400, 300), small, 1500)),
            done(at_origin(300, 200), Rule::Deadline)
        );

        // Beyond the deadline (a slow poll): held for much longer than STABLE, still clipped.
        let mut s = Settling::new((300, 200), Some((800, 600)));
        assert_eq!(
            s.observe(poll(at_origin(400, 300), small, 1000)),
            Decision::Wait
        );
        assert_eq!(
            s.observe(poll(at_origin(400, 300), small, 1700)),
            done(at_origin(300, 200), Rule::Deadline)
        );
        // And the same for a second poll after that.
        assert_eq!(
            s.observe(poll(at_origin(400, 300), small, 1720)),
            done(at_origin(300, 200), Rule::Deadline)
        );
    }

    #[test]
    fn a_fit_is_still_made_on_the_last_poll_before_the_deadline() {
        let small = at_origin(300, 200);
        let grown = at_origin(400, 300);
        let mut s = Settling::new((300, 200), Some((800, 600)));
        assert_eq!(
            s.observe(poll(at_origin(400, 300), small, 1299)),
            Decision::Wait
        );
        assert_eq!(
            s.observe(poll(at_origin(400, 300), small, 1499)),
            Decision::Grow { size: (400, 300) }
        );
        // The deadline has passed by the time the grown output shows: the app's size is inside
        // it, so it is reported whole, not clipped.
        assert_eq!(
            s.observe(poll(at_origin(400, 300), grown, 1700)),
            done(at_origin(400, 300), Rule::Exact)
        );
    }

    #[test]
    fn an_overflowing_size_that_keeps_changing_does_not_grow_the_output() {
        let small = at_origin(300, 200);
        let mut s = Settling::new((300, 200), Some((800, 600)));
        for (i, t) in (0..1480).step_by(20).enumerate() {
            let w = 400 + i32::try_from(i).unwrap();
            assert_eq!(
                s.observe(poll(at_origin(w, 300), small, t)),
                Decision::Wait,
                "at {t} ms"
            );
        }
    }

    #[test]
    fn an_overflow_in_one_dimension_grows_only_that_dimension() {
        // 300x200 requested; the app needs 400 wide but takes 150 high.
        let small = at_origin(300, 200);
        let mut s = Settling::new((300, 200), Some((800, 600)));
        assert_eq!(
            s.observe(poll(at_origin(400, 150), small, 0)),
            Decision::Wait
        );
        assert_eq!(
            s.observe(poll(at_origin(400, 150), small, 200)),
            Decision::Grow { size: (400, 200) }
        );
        // The target is now 400x200. The client follows the taller work area.
        let grown = at_origin(400, 200);
        assert_eq!(
            s.observe(poll(at_origin(400, 150), grown, 220)),
            Decision::Wait
        );
        assert_eq!(
            s.observe(poll(at_origin(400, 200), grown, 240)),
            done(at_origin(400, 200), Rule::Exact)
        );
    }

    #[test]
    fn restarting_discards_the_stability_evidence_and_the_old_baseline() {
        // A late bar squeezes the window to 1200x850 and the stability clock starts. The caller
        // is about to repad, so the window's size now is the baseline and the clock starts over:
        // the stale 1200x850 right after the repad must not complete.
        let padded = at_origin(1200, 950);
        let squeezed = rect(0, 50, 1200, 900);
        let mut s = Settling::new((1200, 900), Some((1000, 800)));
        assert_eq!(
            s.observe(poll(squeezed, at_origin(1200, 900), 20)),
            Decision::Wait
        );
        assert_eq!(
            s.observe(poll(squeezed, at_origin(1200, 900), 120)),
            Decision::Wait
        );
        s.restart(Some((1200, 850)));
        // 280 ms after the first sighting: without the restart this would be Stable.
        assert_eq!(s.observe(poll(squeezed, padded, 300)), Decision::Wait);
        assert_eq!(s.observe(poll(squeezed, padded, 500)), Decision::Wait);
        assert_eq!(
            s.observe(poll(rect(0, 50, 1200, 950), padded, 520)),
            done(rect(0, 50, 1200, 950), Rule::Exact)
        );
    }

    #[test]
    fn a_window_with_nothing_on_the_output_has_no_geometry() {
        let extent = at_origin(1200, 900);
        let mut s = Settling::new((1200, 900), None);
        // Wholly right of the output, and an empty window.
        assert_eq!(
            s.observe(poll(rect(5000, 0, 6200, 900), extent, 1500)),
            Decision::Timeout
        );
        let mut s = Settling::new((1200, 900), Some((1600, 1200)));
        assert_eq!(
            s.observe(poll(at_origin(0, 0), extent, 300)),
            Decision::Wait
        );
        assert_eq!(
            s.observe(poll(at_origin(0, 0), extent, 1500)),
            Decision::Timeout
        );
    }

    #[test]
    fn the_exact_fill_is_checked_on_the_deadline_poll_too() {
        let extent = at_origin(1200, 900);
        let mut s = Settling::new((1200, 900), Some((1600, 1200)));
        assert_eq!(
            s.observe(poll(at_origin(1200, 900), extent, 1500)),
            done(at_origin(1200, 900), Rule::Exact)
        );
    }

    #[test]
    fn output_extent_and_clipping() {
        let monitor = serde_json::json!({"width": 1200, "height": 900});
        assert_eq!(output_extent(&monitor).unwrap(), at_origin(1200, 900));
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"width": 1200}),
            serde_json::json!({"width": 0, "height": 900}),
            serde_json::json!({"width": -1, "height": 900}),
        ] {
            assert!(matches!(
                output_extent(&bad),
                Err(PlatformError::Backend(_))
            ));
        }
        let parked = |content| Parked {
            window: WindowId(1),
            kind: ParkingKind::Twin,
            display: DisplayId(7),
            content,
        };
        let extent = at_origin(1200, 900);
        assert_eq!(
            clipped(parked(at_origin(1600, 1200)), extent).unwrap(),
            parked(at_origin(1200, 900))
        );
        assert_eq!(
            clipped(parked(rect(10, 20, 600, 500)), extent).unwrap(),
            parked(rect(10, 20, 600, 500))
        );
        assert!(clipped(parked(rect(1200, 0, 1400, 900)), extent).is_err());
    }

    #[test]
    fn sizes_from_before_are_in_twin_pixels() {
        let client = serde_json::json!({"size": [800, 600]});
        assert_eq!(client_size(&client), Some((800.0, 600.0)));
        assert_eq!(client_size(&serde_json::json!({})), None);
        assert_eq!(client_size(&serde_json::json!({"size": [800]})), None);
        assert_eq!(device_size((800.0, 600.0), 2.0), Some((1600, 1200)));
        assert_eq!(device_size((800.0, 600.0), 1.25), Some((800, 600)));
        assert_eq!(device_size((0.0, 600.0), 2.0), None);
        assert_eq!(device_size((800.0, -1.0), 2.0), None);
    }

    #[test]
    fn journal_round_trip() {
        let dir = std::env::temp_dir().join(format!("cp-park-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("parking.json");
        let ipc = HyprIpc::new("none", &dir, Duration::from_millis(10));
        let mut p = HyprlandParking::new(ipc.clone(), path.clone()).unwrap();
        p.entries.insert(
            5,
            Entry {
                window: 5,
                address: "0x1".into(),
                output: "CROSSPANE-5".into(),
                workspace: "crosspane-5".into(),
                slot: 0,
                original: Original {
                    workspace: "2".into(),
                    floating: true,
                    at: [1, 2],
                    size: [3, 4],
                    fullscreen: 0,
                },
            },
        );
        p.save().unwrap();
        let q = HyprlandParking::new(ipc, path).unwrap();
        assert_eq!(q.entries, p.entries);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
