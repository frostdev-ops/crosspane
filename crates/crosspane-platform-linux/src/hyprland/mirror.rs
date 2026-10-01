//! M1 mirror parking: leave geometry alone and journal both effective border colours before
//! changing them. Hyprland's `getprop` exposes effective gradients, not override priorities;
//! restoration writes those exact gradients back and checks them before retiring the journal.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crosspane_platform::{Parked, ParkingKind, PlatformError, WindowParking};
use crosspane_types::geom::{PixelRect, PixelSize, euclid::point2};
use crosspane_types::id::{DisplayId, WindowId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ipc::HyprIpc;

const PROPS: [&str; 2] = ["active_border_color", "inactive_border_color"];

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    window: u64,
    address: String,
    borders: [String; 2],
}

/// Mirror a window without moving or resizing its source.
#[derive(Debug)]
pub struct HyprlandMirrorParking {
    ipc: HyprIpc,
    journal: PathBuf,
    border: String,
    entries: BTreeMap<u64, Entry>,
}

impl HyprlandMirrorParking {
    /// Load the journal; call `recover()` before starting a new projection.
    pub fn new(ipc: HyprIpc, journal: PathBuf, border: &str) -> Result<Self, PlatformError> {
        let mut entries = BTreeMap::new();
        match std::fs::read(&journal) {
            Ok(bytes) => {
                let loaded: Vec<Entry> = serde_json::from_slice(&bytes).map_err(backend)?;
                for entry in loaded {
                    validate_address(&entry.address)?;
                    for value in &entry.borders {
                        gradient_value(value)?;
                    }
                    if entries.insert(entry.window, entry).is_some() {
                        return Err(backend("duplicate mirror journal window"));
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(backend(e)),
        }
        Ok(Self {
            ipc,
            journal,
            border: border.to_owned(),
            entries,
        })
    }

    fn client(&self, window: WindowId) -> Result<Option<Value>, PlatformError> {
        let clients = self.ipc.json("clients")?;
        let clients = clients
            .as_array()
            .ok_or_else(|| backend("clients is not a list"))?;
        Ok(clients
            .iter()
            .find(|c| {
                c.get("stableId")
                    .and_then(Value::as_str)
                    .and_then(|id| u64::from_str_radix(id, 16).ok())
                    == Some(window.0)
            })
            .cloned())
    }

    fn borders(&self, address: &str) -> Result<[String; 2], PlatformError> {
        validate_address(address)?;
        let mut result = [String::new(), String::new()];
        for (i, prop) in PROPS.iter().enumerate() {
            let text = self
                .ipc
                .request(&format!("getprop address:{address} {prop}"))?;
            let text = text.trim().to_owned();
            gradient_value(&text)?;
            result[i] = text;
        }
        Ok(result)
    }

    /// Resolve identity inside the same Lua request as each mutation. Addresses can be reused
    /// after a window closes, so an earlier clients snapshot alone is insufficient.
    fn set(&self, entry: &Entry, prop: &str, value: &str) -> Result<(), PlatformError> {
        self.ipc.eval(&format!(
            "local w = hl.get_window(\"address:{}\"); if w and w.stable_id == {} then hl.dispatch(hl.dsp.window.set_prop({{ window = \"address:{}\", prop = \"{}\", value = \"{}\" }})) end",
            lua_escape(&entry.address), entry.window, lua_escape(&entry.address), prop, lua_escape(value)
        ))
    }

    fn save(&self, entries: &BTreeMap<u64, Entry>) -> Result<(), PlatformError> {
        let bytes =
            serde_json::to_vec_pretty(&entries.values().collect::<Vec<_>>()).map_err(backend)?;
        let tmp = self.journal.with_extension("tmp");
        let mut file = std::fs::File::create(&tmp).map_err(backend)?;
        file.write_all(&bytes).map_err(backend)?;
        file.sync_all().map_err(backend)?;
        std::fs::rename(tmp, &self.journal).map_err(backend)?;
        let dir = self
            .journal
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::File::open(dir)
            .map_err(backend)?
            .sync_all()
            .map_err(backend)?;
        Ok(())
    }

    fn undo(&mut self, window: WindowId) -> Result<(), PlatformError> {
        let Some(entry) = self.entries.get(&window.0).cloned() else {
            return Ok(());
        };
        if let Some(client) = self.client(window)? {
            if client.get("address").and_then(Value::as_str) != Some(entry.address.as_str()) {
                return Err(backend("mirror journal identity changed"));
            }
            for (prop, original) in PROPS.iter().zip(&entry.borders) {
                self.set(&entry, prop, &gradient_value(original)?)?;
            }
            // A closed window needs no restoration; never read a reused address's properties.
            if let Some(client) = self.client(window)? {
                if client.get("address").and_then(Value::as_str) != Some(entry.address.as_str()) {
                    return Err(backend("mirror journal identity changed"));
                }
                if self.borders(&entry.address)? != entry.borders {
                    return Err(backend("mirror border restoration did not round trip"));
                }
            }
        }
        let mut remaining = self.entries.clone();
        remaining.remove(&window.0);
        self.save(&remaining)?;
        self.entries = remaining;
        Ok(())
    }
}

impl WindowParking for HyprlandMirrorParking {
    fn park(
        &mut self,
        window: WindowId,
        _size: PixelSize,
        _scale: f64,
    ) -> Result<Parked, PlatformError> {
        if self.entries.contains_key(&window.0) {
            return self.geometry(window);
        }
        let client = self.client(window)?.ok_or(PlatformError::NotFound)?;
        geometry(&self.ipc, window, &client)?;
        let marker = marker_gradient(&self.border)?;
        let marker_value = gradient_value(&marker)?;
        let address = client
            .get("address")
            .and_then(Value::as_str)
            .ok_or_else(|| backend("client has no address"))?
            .to_owned();
        let entry = Entry {
            window: window.0,
            borders: self.borders(&address)?,
            address,
        };
        let mut entries = self.entries.clone();
        entries.insert(window.0, entry.clone());
        self.save(&entries)?;
        self.entries = entries;
        let result = (|| {
            for prop in PROPS {
                self.set(&entry, prop, &marker_value)?;
            }
            // Resolve again: the client may have closed while applying its border.
            let parked = self.geometry(window)?;
            if self.borders(&entry.address)? != [marker.clone(), marker.clone()] {
                return Err(backend("mirror border marker did not round trip"));
            }
            Ok(parked)
        })();
        if result.is_err()
            && let Err(e) = self.undo(window)
        {
            tracing::warn!(error = %e, "failed mirror park rollback; journal retained for recovery");
        }
        result
    }

    fn resize(
        &mut self,
        window: WindowId,
        _size: PixelSize,
        _scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.geometry(window)
    }

    fn geometry(&self, window: WindowId) -> Result<Parked, PlatformError> {
        let entry = self.entries.get(&window.0).ok_or(PlatformError::NotFound)?;
        let client = self.client(window)?.ok_or(PlatformError::NotFound)?;
        if client.get("address").and_then(Value::as_str) != Some(entry.address.as_str()) {
            return Err(backend("mirror journal identity changed"));
        }
        geometry(&self.ipc, window, &client)
    }

    fn restore(&mut self, window: WindowId) -> Result<(), PlatformError> {
        self.undo(window)
    }

    fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
        let windows: Vec<_> = self.entries.keys().copied().map(WindowId).collect();
        let mut restored = Vec::new();
        let mut failure = None;
        for window in windows {
            match self.undo(window) {
                Ok(()) => restored.push(window),
                Err(e) => {
                    failure = Some(e);
                }
            }
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(restored),
        }
    }
}

fn geometry(ipc: &HyprIpc, window: WindowId, client: &Value) -> Result<Parked, PlatformError> {
    let id = client
        .get("monitor")
        .and_then(Value::as_u64)
        .ok_or_else(|| backend("client has no monitor"))?;
    let monitors = ipc.json("monitors")?;
    let monitor = monitors
        .as_array()
        .ok_or_else(|| backend("monitors is not a list"))?
        .iter()
        .find(|m| m.get("id").and_then(Value::as_u64) == Some(id))
        .ok_or(PlatformError::NotFound)?;
    parked_from(window, client, monitor)
}

fn parked_from(window: WindowId, client: &Value, monitor: &Value) -> Result<Parked, PlatformError> {
    let number = |v: Option<&Value>| {
        v.and_then(Value::as_f64)
            .filter(|v| v.is_finite())
            .ok_or_else(|| backend("invalid mirror geometry"))
    };
    let scale = number(monitor.get("scale"))?;
    if scale <= 0.0 {
        return Err(backend("invalid monitor scale"));
    }
    let px = |v: f64| -> Result<i32, PlatformError> {
        let value = (v * scale).round();
        if value < f64::from(i32::MIN) || value > f64::from(i32::MAX) || !value.is_finite() {
            return Err(backend("mirror geometry exceeds pixel coordinates"));
        }
        Ok(value as i32)
    };
    let pair = |key: &str, i: usize| {
        number(
            client
                .get(key)
                .and_then(Value::as_array)
                .and_then(|a| a.get(i)),
        )
    };
    let x = px(pair("at", 0)? - number(monitor.get("x"))?)?;
    let y = px(pair("at", 1)? - number(monitor.get("y"))?)?;
    let w = px(pair("size", 0)?)?;
    let h = px(pair("size", 1)?)?;
    if w <= 0 || h <= 0 {
        return Err(backend("invalid mirror content size"));
    }
    let max_x = x
        .checked_add(w)
        .ok_or_else(|| backend("mirror geometry overflow"))?;
    let max_y = y
        .checked_add(h)
        .ok_or_else(|| backend("mirror geometry overflow"))?;
    let id = monitor
        .get("id")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| backend("invalid monitor id"))?;
    Ok(Parked {
        window,
        kind: ParkingKind::Mirror,
        display: DisplayId(id),
        content: PixelRect::new(point2(x, y), point2(max_x, max_y)),
    })
}

fn validate_address(address: &str) -> Result<(), PlatformError> {
    if address
        .strip_prefix("0x")
        .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        Ok(())
    } else {
        Err(backend("invalid mirror window address"))
    }
}

/// 0.56.2's multi-token setter skips token zero. Supply that token explicitly, then the
/// exact ARGB colours and angle returned by getprop. `-1` is an empty gradient, not reset.
fn gradient_value(text: &str) -> Result<String, PlatformError> {
    let mut tokens: Vec<_> = text.split_whitespace().collect();
    let angle = tokens
        .pop()
        .ok_or_else(|| backend("empty border property"))?;
    if angle
        .strip_suffix("deg")
        .and_then(|a| a.parse::<i32>().ok())
        .is_none()
        || tokens
            .iter()
            .any(|c| c.len() != 8 || !c.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(backend("invalid border property"));
    }
    Ok(format!(
        "gradient {}{}",
        tokens.iter().map(|c| format!("0x{c} ")).collect::<String>(),
        angle
    ))
}

/// Accept hexadecimal Hyprland colour forms and integer-angle gradients. Reject syntax
/// we cannot verify rather than let set_prop silently replace a marker with an empty gradient.
fn marker_gradient(text: &str) -> Result<String, PlatformError> {
    let mut colors = Vec::new();
    let mut angle = 0;
    let mut saw_angle = false;
    for token in text.split_whitespace() {
        if let Some(degrees) = token.strip_suffix("deg") {
            if saw_angle {
                return Err(backend("duplicate mirror border angle"));
            }
            angle = degrees.parse::<i32>().map_err(backend)?;
            saw_angle = true;
            continue;
        }
        if saw_angle {
            return Err(backend("mirror border colour after angle"));
        }
        let color = if let Some(hex) = token.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')'))
        {
            if hex.len() != 6 {
                return Err(backend("invalid RGB mirror border"));
            }
            format!("ff{hex}")
        } else if let Some(hex) = token
            .strip_prefix("rgba(")
            .and_then(|s| s.strip_suffix(')'))
        {
            if hex.len() != 8 {
                return Err(backend("invalid RGBA mirror border"));
            }
            // Validate ASCII before slicing an externally supplied colour.
            if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(backend("invalid RGBA mirror border"));
            }
            format!("{}{}", &hex[6..], &hex[..6])
        } else if let Some(hex) = token.strip_prefix("0x") {
            if hex.len() != 8 {
                return Err(backend("invalid ARGB mirror border"));
            }
            hex.to_owned()
        } else {
            return Err(backend("unsupported mirror border colour syntax"));
        };
        if !color.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(backend("invalid mirror border colour"));
        }
        colors.push(color.to_ascii_lowercase());
    }
    if colors.is_empty() {
        return Err(backend("mirror border requires a colour"));
    }
    Ok(format!("{} {angle}deg", colors.join(" ")))
}

fn lua_escape(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\\' => "\\\\".to_owned(),
            '"' => "\\\"".to_owned(),
            c if c.is_control() => format!("\\{:03}", u32::from(c)),
            c => c.to_string(),
        })
        .collect()
}

fn backend(error: impl std::fmt::Display) -> PlatformError {
    PlatformError::Backend(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn geometry_scale_one() {
        check_geometry(1.0, (10, 20, 810, 620));
    }
    #[test]
    fn geometry_scale_two() {
        check_geometry(2.0, (20, 40, 1620, 1240));
    }
    fn check_geometry(scale: f64, (x, y, right, bottom): (i32, i32, i32, i32)) {
        let client = serde_json::json!({"at": [110, -80], "size": [800, 600]});
        let monitor = serde_json::json!({"id": 7, "x": 100, "y": -100, "scale": scale});
        let parked = parked_from(WindowId(1), &client, &monitor).unwrap();
        assert_eq!(parked.kind, ParkingKind::Mirror);
        assert_eq!(parked.display, DisplayId(7));
        assert_eq!(
            parked.content,
            PixelRect::new(point2(x, y), point2(right, bottom))
        );
    }
}
