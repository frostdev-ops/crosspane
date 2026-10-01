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

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crosspane_platform::{Parked, ParkingKind, PlatformError, WindowParking};
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
    fn set_mode(&self, entry: &Entry, size: PixelSize, scale: f64) -> Result<(), PlatformError> {
        let reserved = self.reserved(&entry.output)?;
        self.set_mode_padded(entry, size, scale, reserved)
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
            self.ipc.dispatch(&format!(
                "hl.dsp.window.fullscreen_state({{ window = \"address:{address}\", internal = 0, client = 0 }})"
            ))?;
            self.move_window(&address, &workspace_selector(&entry.original.workspace))?;
            let o = &entry.original;
            if o.fullscreen != 0 {
                self.ipc.dispatch(&format!(
                    "hl.dsp.window.fullscreen_state({{ window = \"address:{address}\", internal = {0}, client = {0} }})",
                    o.fullscreen
                ))?;
            }
            if o.floating {
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
        // Journal first: from here on a crash is recoverable.
        self.entries.insert(window.0, entry.clone());
        self.save()?;
        let result = (|| {
            // The rule comes first, so the new output's first workspace is ours (no stray ones).
            self.ipc.eval(&format!(
                "hl.workspace_rule({{ workspace = \"name:{}\", monitor = \"{}\", default = true, gaps_in = 0, gaps_out = 0, border_size = 0, no_rounding = true, no_shadow = true, decorate = false }})",
                entry.workspace, entry.output
            ))?;
            if self.monitor(&entry.output)?.is_none() {
                expect_ok(
                    &self
                        .ipc
                        .request(&format!("output create headless {}", entry.output))?,
                )?;
            }
            self.set_mode(&entry, size, scale)?;
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
            self.settle(window, size, scale)
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
        self.set_mode(&entry, size, scale)?;
        self.settle(window, size, scale)
    }

    fn geometry(&self, window: WindowId) -> Result<Parked, PlatformError> {
        let entry = self.entries.get(&window.0).ok_or(PlatformError::NotFound)?;
        let client = self.client(window)?.ok_or(PlatformError::NotFound)?;
        let monitor = self
            .monitor(&entry.output)?
            .ok_or(PlatformError::NotFound)?;
        parked_from(window, &client, &monitor)
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
    /// Wait until the window sits on its twin output, then report its geometry.
    fn settle(
        &self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        let entry = self.entries.get(&window.0).ok_or(PlatformError::NotFound)?;
        let (w, h) = mode_size(size, sane_scale(scale));
        let deadline = Instant::now() + SETTLE;
        let mut padded_for = self.reserved(&entry.output)?;
        loop {
            // A bar can arrive on the new output after its mode was set: pad again.
            let reserved = self.reserved(&entry.output)?;
            if reserved != padded_for {
                self.set_mode_padded(entry, size, scale, reserved)?;
                padded_for = reserved;
            }
            let client = self.client(window)?.ok_or(PlatformError::NotFound)?;
            let monitor = self
                .monitor(&entry.output)?
                .ok_or(PlatformError::NotFound)?;
            let parked = parked_from(window, &client, &monitor)?;
            let on_twin = client.pointer("/workspace/name").and_then(Value::as_str)
                == Some(entry.workspace.as_str());
            let content = parked.content;
            let filled = content.width() >= i32::try_from(w).unwrap_or(i32::MAX) - 2
                && content.height() >= i32::try_from(h).unwrap_or(i32::MAX) - 2;
            if on_twin && (filled || Instant::now() >= deadline) {
                // An app may refuse a size; the geometry is what it took.
                return Ok(parked);
            }
            if Instant::now() >= deadline {
                return Err(PlatformError::Timeout);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
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
