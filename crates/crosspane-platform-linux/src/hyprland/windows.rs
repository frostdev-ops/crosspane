//! E2 window enumeration and ordered change notifications from Hyprland's client IPC.

use std::collections::BTreeMap;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crosspane_platform::{
    EventSink, PlatformError, WindowEvent, WindowInfo, WindowRole, WindowSource, WindowState,
};
use crosspane_types::geom::{PointLogical, RectLogical, SizeLogical};
use crosspane_types::id::{DisplayId, WindowId};
use serde_json::Value;

use super::ipc::{EventStream, HyprIpc, IpcEvent};

const DEBOUNCE: Duration = Duration::from_millis(50);
/// How often the window list is diffed when no event arrives.
const POLL: Duration = Duration::from_secs(1);
const PARKING_PREFIX: &str = "special:crosspane-";

type Clients = BTreeMap<WindowId, Client>;
type Addresses = Arc<Mutex<BTreeMap<WindowId, String>>>;

/// Hyprland's projectable windows, identified by `stableId`, rather than a reusable address.
#[derive(Debug)]
pub struct HyprlandWindows {
    ipc: HyprIpc,
    addresses: Addresses,
    events: Option<EventStream>,
    worker: Option<JoinHandle<()>>,
}

impl HyprlandWindows {
    pub fn new(ipc: HyprIpc) -> Result<HyprlandWindows, PlatformError> {
        let clients = snapshot(&ipc)?;
        let addresses = Arc::new(Mutex::new(address_map(&clients)));
        Ok(Self {
            ipc,
            addresses,
            events: None,
            worker: None,
        })
    }

    /// The Hyprland address (`0x…`) of a known projectable window, for dispatchers.
    pub fn address(&self, window: WindowId) -> Option<String> {
        self.addresses.lock().ok()?.get(&window).cloned()
    }

    fn snapshot(&self) -> Result<Clients, PlatformError> {
        let clients = snapshot(&self.ipc)?;
        update_addresses(&self.addresses, &clients)?;
        Ok(clients)
    }
}

impl WindowSource for HyprlandWindows {
    fn windows(&self) -> Result<Vec<WindowInfo>, PlatformError> {
        Ok(self.snapshot()?.into_values().map(|c| c.info).collect())
    }

    fn focused(&self) -> Result<Option<WindowId>, PlatformError> {
        let active = self.ipc.json("activewindow")?;
        if active.as_object().is_some_and(|o| o.is_empty()) {
            return Ok(None);
        }
        Ok(parse_client(&active)?.map(|c| c.info.id))
    }

    fn activate(&mut self, window: WindowId) -> Result<(), PlatformError> {
        // A cached address could now belong to a closed or parked window. Re-check the source
        // before dispatching; Crosspane's parking workspaces are excluded from this snapshot.
        let clients = self.snapshot()?;
        let client = clients.get(&window).ok_or(PlatformError::NotFound)?;
        self.ipc.dispatch(&format!(
            r#"hl.dsp.focus({{ window = "address:{}" }})"#,
            client.address
        ))
    }

    fn subscribe(&mut self, sink: Arc<dyn EventSink<WindowEvent>>) -> Result<(), PlatformError> {
        if self.events.is_some() {
            return Err(PlatformError::Backend(
                "WindowSource::subscribe called twice".into(),
            ));
        }
        let initial = self.snapshot()?;
        let (tx, rx) = mpsc::channel();
        let events = self.ipc.events(Box::new(move |event| {
            let change = match event {
                IpcEvent::Connected => Change::Connected,
                IpcEvent::Event { name, .. } if is_window_event(name) => Change::Refresh,
                IpcEvent::Event {
                    name: "activewindowv2",
                    data,
                } => Change::Focused(parse_hex(data).filter(|address| *address != 0)),
                _ => return,
            };
            let _ = tx.send(change);
        }))?;
        let ipc = self.ipc.clone();
        let addresses = self.addresses.clone();
        let worker = std::thread::Builder::new()
            .name("hypr-windows".into())
            .spawn(move || worker_loop(&ipc, &addresses, initial, &rx, &*sink))
            .map_err(|e| PlatformError::Backend(format!("spawn windows thread: {e}")))?;
        self.events = Some(events);
        self.worker = Some(worker);
        Ok(())
    }
}

impl Drop for HyprlandWindows {
    fn drop(&mut self) {
        // Closing the event stream also closes the worker's channel. Join both so the sink is
        // never called after the subscription has been dropped.
        drop(self.events.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Debug)]
struct Client {
    info: WindowInfo,
    address: String,
}

#[derive(Debug)]
enum Change {
    Connected,
    Refresh,
    Focused(Option<u64>),
}

fn is_window_event(name: &str) -> bool {
    matches!(
        name,
        "openwindow"
            | "closewindow"
            | "windowtitlev2"
            | "movewindowv2"
            | "changefloatingmode"
            | "fullscreen"
            | "minimized"
    )
}

fn worker_loop(
    ipc: &HyprIpc,
    addresses: &Addresses,
    mut clients: Clients,
    rx: &mpsc::Receiver<Change>,
    sink: &dyn EventSink<WindowEvent>,
) {
    for client in clients.values() {
        sink.send(WindowEvent::Added(client.info.clone()));
    }
    loop {
        // Hyprland announces no event when a window's size or position changes by itself (for
        // example when a bar reserves space on its output), so a periodic diff catches those.
        let change = match rx.recv_timeout(POLL) {
            Ok(change) => change,
            Err(RecvTimeoutError::Timeout) => {
                refresh(ipc, addresses, &mut clients, sink);
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        match change {
            Change::Connected => refresh(ipc, addresses, &mut clients, sink),
            Change::Refresh => {
                // Bound the coalescing window even when the compositor emits a continuous stream.
                // Keep every focus event, in order, until the diff has introduced any new IDs.
                let deadline = Instant::now() + DEBOUNCE;
                let mut focuses = Vec::new();
                loop {
                    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Ok(Change::Focused(address)) => focuses.push(address),
                        Ok(Change::Refresh | Change::Connected) => {}
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                    if Instant::now() >= deadline {
                        break;
                    }
                }
                refresh(ipc, addresses, &mut clients, sink);
                for address in focuses {
                    sink.send(WindowEvent::Focused(focus_id(&clients, address)));
                }
            }
            Change::Focused(address) => {
                if address.is_some() && focus_id(&clients, address).is_none() {
                    refresh(ipc, addresses, &mut clients, sink);
                }
                sink.send(WindowEvent::Focused(focus_id(&clients, address)));
            }
        }
    }
}

fn focus_id(clients: &Clients, address: Option<u64>) -> Option<WindowId> {
    let address = address?;
    clients
        .iter()
        .find_map(|(id, c)| (parse_hex(&c.address) == Some(address)).then_some(*id))
}

fn refresh(
    ipc: &HyprIpc,
    addresses: &Addresses,
    previous: &mut Clients,
    sink: &dyn EventSink<WindowEvent>,
) {
    let current = match snapshot(ipc).and_then(|clients| {
        update_addresses(addresses, &clients)?;
        Ok(clients)
    }) {
        Ok(current) => current,
        Err(e) => {
            // A failed query is not evidence that a window closed. Connected will re-diff after
            // a reconnect; subsequent window events also retry the query.
            tracing::warn!(error = %e, "hyprland clients query failed");
            return;
        }
    };
    for id in previous.keys() {
        if !current.contains_key(id) {
            sink.send(WindowEvent::Removed(*id));
        }
    }
    for (id, client) in &current {
        match previous.get(id) {
            None => sink.send(WindowEvent::Added(client.info.clone())),
            Some(old) if old.info != client.info => {
                sink.send(WindowEvent::Changed(client.info.clone()));
            }
            Some(_) => {}
        }
    }
    *previous = current;
}

fn address_map(clients: &Clients) -> BTreeMap<WindowId, String> {
    clients
        .iter()
        .map(|(id, c)| (*id, c.address.clone()))
        .collect()
}

fn update_addresses(addresses: &Addresses, clients: &Clients) -> Result<(), PlatformError> {
    *addresses
        .lock()
        .map_err(|_| PlatformError::Backend("hyprland window address map poisoned".into()))? =
        address_map(clients);
    Ok(())
}

fn snapshot(ipc: &HyprIpc) -> Result<Clients, PlatformError> {
    parse_clients(&ipc.json("clients")?)
}

fn parse_clients(json: &Value) -> Result<Clients, PlatformError> {
    let list = json
        .as_array()
        .ok_or_else(|| PlatformError::Backend("hyprland clients: not a list".into()))?;
    let mut clients = Clients::new();
    for value in list {
        if let Some(client) = parse_client(value)?
            && clients.insert(client.info.id, client).is_some()
        {
            return Err(PlatformError::Backend(
                "hyprland clients: duplicate stableId".into(),
            ));
        }
    }
    Ok(clients)
}

fn parse_client(c: &Value) -> Result<Option<Client>, PlatformError> {
    let bad = |key| PlatformError::Backend(format!("hyprland client: bad or missing `{key}`"));
    let mapped = c["mapped"].as_bool().ok_or_else(|| bad("mapped"))?;
    if !mapped
        || c["workspace"]["name"]
            .as_str()
            .is_some_and(|name| name.starts_with(PARKING_PREFIX))
    {
        return Ok(None);
    }
    let string = |key| c[key].as_str().ok_or_else(|| bad(key));
    let id = WindowId(parse_hex(string("stableId")?).ok_or_else(|| bad("stableId"))?);
    let address = string("address")?;
    if !address.starts_with("0x") || !parse_hex(address).is_some_and(|a| a != 0) {
        return Err(bad("address"));
    }
    let pair = |key| -> Result<[f64; 2], PlatformError> {
        let values = c[key].as_array().ok_or_else(|| bad(key))?;
        if values.len() != 2 {
            return Err(bad(key));
        }
        let number = |index: usize| {
            values[index]
                .as_f64()
                .filter(|n| n.is_finite())
                .ok_or_else(|| bad(key))
        };
        Ok([number(0)?, number(1)?])
    };
    let [x, y] = pair("at")?;
    let [width, height] = pair("size")?;
    if width < 0.0 || height < 0.0 {
        return Err(bad("size"));
    }
    let class = string("class")?;
    let app_id = if class.is_empty() {
        string("initialClass")?
    } else {
        class
    };
    let hidden = c["hidden"].as_bool().ok_or_else(|| bad("hidden"))?;
    let fullscreen = c["fullscreen"].as_u64().ok_or_else(|| bad("fullscreen"))?;
    let parent_address = match &c["parent"] {
        Value::String(s) => parse_hex(s),
        Value::Number(n) => n.as_u64(),
        Value::Object(p) => p.get("address").and_then(Value::as_str).and_then(parse_hex),
        _ => None,
    };
    Ok(Some(Client {
        info: WindowInfo {
            id,
            title: string("title")?.to_owned(),
            app_id: app_id.to_owned(),
            pid: c["pid"].as_u64().and_then(|pid| u32::try_from(pid).ok()),
            display: c["monitor"]
                .as_u64()
                .and_then(|id| u32::try_from(id).ok())
                .map(DisplayId),
            frame: RectLogical::new(PointLogical::new(x, y), SizeLogical::new(width, height)),
            state: if hidden {
                WindowState::Hidden
            } else if fullscreen > 0 {
                WindowState::Fullscreen
            } else {
                WindowState::Normal
            },
            role: if parent_address.is_some_and(|address| address != 0) {
                WindowRole::Dialog
            } else {
                WindowRole::Toplevel
            },
            parent: None,
        },
        address: address.to_owned(),
    }))
}

fn parse_hex(value: &str) -> Option<u64> {
    let value = value.strip_prefix("0x").unwrap_or(value);
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(value, 16).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use serde_json::json;

    fn client() -> Value {
        json!({
            "stableId": "1800000c", "address": "0x60aba49bf930", "mapped": true,
            "hidden": false, "at": [-120, 45], "size": [509, 895],
            "workspace": {"id": 1, "name": "1"}, "monitor": 2,
            "class": "foot", "initialClass": "initial-foot", "title": "terminal",
            "pid": 469303, "fullscreen": 0
        })
    }

    #[test]
    fn maps_client_json() {
        let parsed = parse_clients(&json!([client()])).unwrap();
        let c = parsed.get(&WindowId(0x1800000c)).unwrap();
        assert_eq!(c.address, "0x60aba49bf930");
        assert_eq!(
            c.info,
            WindowInfo {
                id: WindowId(0x1800000c),
                title: "terminal".into(),
                app_id: "foot".into(),
                pid: Some(469303),
                display: Some(DisplayId(2)),
                frame: RectLogical::new(
                    PointLogical::new(-120.0, 45.0),
                    SizeLogical::new(509.0, 895.0)
                ),
                state: WindowState::Normal,
                role: WindowRole::Toplevel,
                parent: None,
            }
        );

        let mut variant = client();
        variant["class"] = json!("");
        variant["fullscreen"] = json!(2);
        variant["parent"] = json!("0x123");
        variant["monitor"] = json!(-1);
        variant["pid"] = Value::Null;
        let c = parse_client(&variant).unwrap().unwrap();
        assert_eq!(c.info.app_id, "initial-foot");
        assert_eq!(c.info.state, WindowState::Fullscreen);
        assert_eq!(c.info.role, WindowRole::Dialog);
        assert_eq!(c.info.parent, None);
        assert_eq!(c.info.display, None);
        assert_eq!(c.info.pid, None);

        variant["hidden"] = json!(true);
        assert_eq!(
            parse_client(&variant).unwrap().unwrap().info.state,
            WindowState::Hidden
        );
    }

    #[test]
    fn skips_unmapped_and_crosspane_parking_json() {
        let mut parked = client();
        parked["workspace"]["name"] = json!("special:crosspane-42");
        assert!(
            parse_clients(&json!([{"mapped": false}, parked]))
                .unwrap()
                .is_empty()
        );

        let mut other_special = client();
        other_special["workspace"]["name"] = json!("special:scratchpad");
        assert_eq!(parse_clients(&json!([other_special])).unwrap().len(), 1);
    }

    #[test]
    fn rejects_invalid_client_json() {
        assert!(parse_clients(&json!({})).is_err());
        for (field, value) in [
            ("stableId", json!("not-hex")),
            ("address", json!("0x1\"; injected()")),
            ("at", json!([0])),
            ("size", json!([-1, 2])),
        ] {
            let mut invalid = client();
            invalid[field] = value;
            assert!(parse_clients(&json!([invalid])).is_err(), "{field}");
        }
        assert!(parse_clients(&json!([client(), client()])).is_err());
    }
}
