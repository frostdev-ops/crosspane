//! IPC runs away from the Wayland thread so a slow query cannot delay gate cleanup.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use crosspane_platform::PlatformError;
use crosspane_types::input::LockKeys;
use serde_json::Value;

use super::super::ipc::{HyprIpc, IpcEvent};
use super::{CONNECT_BUDGET, backend, receive};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Rmlvo {
    pub layout: String,
    pub variant: String,
    pub options: String,
}

pub(super) struct Config {
    pub names: Rmlvo,
    pub active_keymap: Option<String>,
    pub layout_index: Option<u32>,
    pub locks: LockKeys,
    pub monitors: Vec<(String, u32)>,
    pub keyboard_addresses: BTreeSet<String>,
}

fn keyboards(devices: &Value) -> Result<&[Value], PlatformError> {
    devices
        .get("keyboards")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| backend("hyprland devices: missing keyboards list"))
}

fn physical_keyboard<'a>(
    devices: &'a Value,
    own_name: Option<&str>,
) -> Result<Option<&'a Value>, PlatformError> {
    let candidates = keyboards(devices)?.iter().filter(|k| {
        let name = k["name"].as_str().unwrap_or_default();
        // Hyprland names virtual keyboards after their client process. Exclude ours even when
        // Hyprland makes it main, and prefer a physical device over other virtual clients.
        Some(name) != own_name && !name.starts_with("hl-virtual-keyboard")
    });
    Ok(candidates
        .clone()
        .find(|k| k["main"].as_bool() == Some(true))
        .or_else(|| candidates.into_iter().next()))
}

fn rmlvo(
    keyboard: Option<&Value>,
    mut fallback: impl FnMut(&str) -> Result<String, PlatformError>,
) -> Result<Rmlvo, PlatformError> {
    let mut field = |name, option| {
        let value = match keyboard.and_then(|k| k.get(name)).and_then(Value::as_str) {
            Some(value) => value.to_owned(),
            None => fallback(option)?,
        };
        // xkbcommon constructs C strings internally.
        if value.contains('\0') {
            return Err(backend("invalid keyboard layout option"));
        }
        Ok(value)
    };
    Ok(Rmlvo {
        layout: field("layout", "input:kb_layout")?,
        variant: field("variant", "input:kb_variant")?,
        options: field("options", "input:kb_options")?,
    })
}

pub(super) fn read(
    ipc: &HyprIpc,
    own_name: Option<&str>,
    deadline: Instant,
) -> Result<Config, PlatformError> {
    let devices = ipc.json("devices")?;
    let keyboard = physical_keyboard(&devices, own_name)?;
    let names = rmlvo(keyboard, |option| {
        if Instant::now() >= deadline {
            return Err(PlatformError::Timeout);
        }
        ipc.json(&format!("getoption {option}"))?
            .get("str")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| backend("keyboard layout option unavailable"))
    })?;
    let monitors = ipc.monitor_ids()?;
    if Instant::now() >= deadline {
        return Err(PlatformError::Timeout);
    }
    Ok(Config {
        names,
        active_keymap: keyboard
            .and_then(|k| k["active_keymap"].as_str())
            .map(str::to_owned),
        layout_index: keyboard
            .and_then(|k| k["active_layout_index"].as_u64())
            .and_then(|i| u32::try_from(i).ok()),
        locks: parse_locks(&devices, own_name)?,
        monitors,
        keyboard_addresses: keyboards(&devices)?
            .iter()
            .filter_map(|k| k["address"].as_str().map(str::to_owned))
            .collect(),
    })
}

pub(super) fn own_keyboard_name(
    ipc: &HyprIpc,
    previous: &BTreeSet<String>,
) -> Result<String, PlatformError> {
    let devices = ipc.json("devices")?;
    keyboards(&devices)?
        .iter()
        .find(|k| {
            k["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("hl-virtual-keyboard"))
                && k["address"].as_str().is_some_and(|a| !previous.contains(a))
        })
        .and_then(|k| k["name"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| backend("virtual keyboard identity unavailable"))
}

fn parse_locks(devices: &Value, own_name: Option<&str>) -> Result<LockKeys, PlatformError> {
    let keyboard = physical_keyboard(devices, own_name)?;
    Ok(LockKeys {
        caps_lock: keyboard.and_then(|k| k["capsLock"].as_bool()),
        num_lock: keyboard.and_then(|k| k["numLock"].as_bool()),
        scroll_lock: None,
    })
}

struct Request {
    deadline: Instant,
    reply: SyncSender<Result<LockKeys, PlatformError>>,
}

#[derive(Debug)]
pub(super) struct LockReader {
    sender: SyncSender<Request>,
}

impl LockReader {
    pub fn new(ipc: HyprIpc, own_name: String) -> Result<Self, PlatformError> {
        let (sender, requests) = mpsc::sync_channel::<Request>(2);
        std::thread::Builder::new()
            .name("hypr-inject-locks".into())
            .spawn(move || {
                while let Ok(request) = requests.recv() {
                    let result = if Instant::now() >= request.deadline {
                        Err(PlatformError::Timeout)
                    } else {
                        ipc.json("devices")
                            .and_then(|v| parse_locks(&v, Some(&own_name)))
                    };
                    let _ = request.reply.send(result);
                }
            })
            .map_err(|_| backend("could not start lock state reader"))?;
        Ok(Self { sender })
    }
    pub fn read(&self, deadline: Instant) -> Result<LockKeys, PlatformError> {
        let (reply, result) = mpsc::sync_channel(1);
        match self.sender.try_send(Request { deadline, reply }) {
            Ok(()) => receive(&result, deadline)?,
            Err(TrySendError::Full(_)) => Err(PlatformError::Timeout),
            Err(TrySendError::Disconnected(_)) => Err(backend("lock state reader stopped")),
        }
    }
}

pub(super) struct Watcher {
    pub updates: Receiver<Result<Config, PlatformError>>,
    pub refresh: SyncSender<()>,
    stop: Arc<AtomicBool>,
}

impl Watcher {
    pub fn new(ipc: HyprIpc, own_name: String) -> Result<Self, PlatformError> {
        let (refresh, requests) = mpsc::sync_channel(1);
        let (sender, updates) = mpsc::sync_channel(1);
        let dirty = Arc::new(AtomicBool::new(true));
        let changed = dirty.clone();
        let stream = ipc.events(Box::new(move |event| {
            if matches!(
                event,
                IpcEvent::Connected
                    | IpcEvent::Event {
                        name: "activelayout"
                            | "configreloaded"
                            | "monitoradded"
                            | "monitoraddedv2"
                            | "monitorremoved",
                        ..
                    }
            ) {
                changed.store(true, Ordering::Release);
            }
        }))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        std::thread::Builder::new()
            .name("hypr-inject-config".into())
            .spawn(move || {
                // EventStream's join can take 100 ms. Its destruction stays on this IPC worker,
                // outside both the public input calls and the Wayland cleanup deadline.
                let _stream = stream;
                while !stopped.load(Ordering::Acquire) {
                    if dirty.swap(false, Ordering::AcqRel) {
                        let update = read(&ipc, Some(&own_name), Instant::now() + CONNECT_BUDGET);
                        match sender.try_send(update) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => dirty.store(true, Ordering::Release),
                            Err(TrySendError::Disconnected(_)) => break,
                        }
                    }
                    match requests.recv_timeout(Duration::from_millis(10)) {
                        Ok(()) => dirty.store(true, Ordering::Release),
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .map_err(|_| backend("could not start layout watcher"))?;
        Ok(Self {
            updates,
            refresh,
            stop,
        })
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rmlvo_config_parse_and_fallback() {
        let device = json!({"layout":"de,us", "variant":"nodeadkeys,", "options":"compose:caps"});
        let names = rmlvo(Some(&device), |_| panic!("unexpected fallback")).unwrap();
        assert_eq!(names.layout, "de,us");
        assert_eq!(names.options, "compose:caps");
        let names = rmlvo(Some(&json!({"layout":"us"})), |key| {
            Ok(format!("fallback:{key}"))
        })
        .unwrap();
        assert_eq!(names.variant, "fallback:input:kb_variant");
        assert_eq!(names.options, "fallback:input:kb_options");
        assert!(rmlvo(Some(&json!({"layout":"a\u{0}b"})), |_| Ok(String::new())).is_err());
    }

    #[test]
    fn lock_reader_excludes_our_main_virtual_keyboard() {
        let devices = json!({"keyboards":[
            {"name":"hl-virtual-keyboard-test", "main":true, "capsLock":true, "numLock":true},
            {"name":"physical", "main":false, "capsLock":false, "numLock":false}
        ]});
        assert_eq!(
            parse_locks(&devices, Some("hl-virtual-keyboard-test"))
                .unwrap()
                .caps_lock,
            Some(false)
        );
    }
}
