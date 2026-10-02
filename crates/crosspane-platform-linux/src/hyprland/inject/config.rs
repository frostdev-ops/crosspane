//! IPC runs away from the Wayland thread so a slow query cannot delay gate cleanup.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// First delay before asking for another refresh after one failed.
pub(super) const RETRY_MIN: Duration = Duration::from_millis(100);
/// The retry delay doubles per further failure up to this ceiling.
pub(super) const RETRY_MAX: Duration = Duration::from_secs(2);

/// Whether the worker may trust the output layout and keymap it holds.
///
/// A refresh that fails (the watcher could not read the configuration, or the configuration it
/// read could not be applied) leaves the worker not knowing what the compositor looks like now.
/// The worker then *pauses*: it releases whatever it holds and refuses new input until a later
/// refresh is applied. This type is only the decision: it does no I/O and reads no clock, so tests
/// drive it with explicit instants.
#[derive(Debug, Default)]
pub(super) struct Refresh {
    paused: Option<Pause>,
}

#[derive(Debug)]
struct Pause {
    /// The delay that preceded the current `retry_at`; doubles on every further failure.
    backoff: Duration,
    retry_at: Instant,
    /// A refresh was asked for since the last failure and has not answered yet.
    requested: bool,
}

impl Refresh {
    pub fn is_paused(&self) -> bool {
        self.paused.is_some()
    }

    /// A refresh failed at `now`. Returns true when this call paused the worker, so the caller
    /// releases held input and logs once. A failure while already paused only lengthens the wait.
    pub fn failed(&mut self, now: Instant) -> bool {
        match &mut self.paused {
            Some(pause) => {
                pause.backoff = (pause.backoff * 2).min(RETRY_MAX);
                pause.retry_at = now + pause.backoff;
                pause.requested = false;
                false
            }
            None => {
                self.paused = Some(Pause {
                    backoff: RETRY_MIN,
                    retry_at: now + RETRY_MIN,
                    requested: false,
                });
                true
            }
        }
    }

    /// A fresh, valid configuration was applied in full. Returns true when that resumed a paused
    /// worker. The next pause starts again at [`RETRY_MIN`].
    pub fn applied(&mut self) -> bool {
        self.paused.take().is_some()
    }

    /// Whether the caller should ask the watcher for another refresh now. True once per failure
    /// and only after its delay; the answer (success or failure) re-arms it.
    pub fn retry_due(&mut self, now: Instant) -> bool {
        match &mut self.paused {
            Some(pause) if !pause.requested && now >= pause.retry_at => {
                pause.requested = true;
                true
            }
            _ => false,
        }
    }
}

/// One answer from the watcher.
pub(super) struct Update {
    /// The configuration epoch when the watcher *started* reading. Invalidations counted since
    /// then (see [`note_event`], and `Source::config_epoch`) may not be in `config`.
    pub epoch: u64,
    pub config: Result<Config, PlatformError>,
}

/// Events after which the configuration (outputs, keyboard layout, options) may differ from any
/// snapshot read before them. Every one of them marks the watcher dirty.
fn invalidates(event: &IpcEvent<'_>) -> bool {
    matches!(
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
    )
}

/// What the event thread does for each event. A configuration change counts in the epoch as well
/// as marking the watcher dirty, so a snapshot whose read began before it can be told apart from
/// one that began after, even when the outputs did not change (a keyboard layout change).
pub(super) fn note_event(event: &IpcEvent<'_>, dirty: &AtomicBool, epoch: &AtomicU64) {
    if invalidates(event) {
        // The epoch first: a read that starts once the flag is seen carries the new value.
        epoch.fetch_add(1, Ordering::AcqRel);
        dirty.store(true, Ordering::Release);
    }
}

/// Whether nothing invalidated the configuration since a snapshot stamped `stamp` began.
pub(super) fn snapshot_is_current(stamp: u64, epoch: &AtomicU64) -> bool {
    epoch.load(Ordering::Acquire) == stamp
}

pub(super) struct Watcher {
    pub updates: Receiver<Update>,
    pub refresh: SyncSender<()>,
    stop: Arc<AtomicBool>,
}

impl Watcher {
    /// `epoch` is shared with the worker, which bumps it on every output change it sees, and with
    /// the event thread below, which bumps it on every configuration event; each read is stamped
    /// with its value from before the read began.
    pub fn new(
        ipc: HyprIpc,
        own_name: String,
        epoch: Arc<AtomicU64>,
    ) -> Result<Self, PlatformError> {
        let (refresh, requests) = mpsc::sync_channel(1);
        let (sender, updates) = mpsc::sync_channel(1);
        let dirty = Arc::new(AtomicBool::new(true));
        let changed = dirty.clone();
        let invalidations = epoch.clone();
        let stream = ipc.events(Box::new(move |event| {
            note_event(&event, &changed, &invalidations);
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
                        let epoch = epoch.load(Ordering::Acquire);
                        let config = read(&ipc, Some(&own_name), Instant::now() + CONNECT_BUDGET);
                        match sender.try_send(Update { epoch, config }) {
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

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_failed_refresh_pauses_and_schedules_the_first_retry() {
        let t0 = Instant::now();
        let mut refresh = Refresh::default();
        assert!(!refresh.is_paused());
        assert!(!refresh.retry_due(t0 + ms(10_000)), "nothing to retry yet");
        assert!(refresh.failed(t0), "the first failure pauses");
        assert!(refresh.is_paused());
        assert!(!refresh.retry_due(t0), "never immediately");
        assert!(!refresh.retry_due(t0 + ms(99)));
        assert!(refresh.retry_due(t0 + ms(100)));
    }

    #[test]
    fn a_retry_is_requested_once_until_its_answer() {
        let t0 = Instant::now();
        let mut refresh = Refresh::default();
        refresh.failed(t0);
        assert!(refresh.retry_due(t0 + ms(100)));
        assert!(!refresh.retry_due(t0 + ms(100)));
        assert!(
            !refresh.retry_due(t0 + ms(60_000)),
            "no new request while one is unanswered"
        );
        // The answer was another failure: not newly paused, and the next retry is later.
        let t1 = t0 + ms(150);
        assert!(!refresh.failed(t1));
        assert!(refresh.is_paused());
        assert!(!refresh.retry_due(t1 + ms(199)));
        assert!(refresh.retry_due(t1 + ms(200)));
    }

    #[test]
    fn repeated_failures_back_off_to_the_cap() {
        let mut now = Instant::now();
        let mut refresh = Refresh::default();
        assert!(refresh.failed(now));
        let mut delays = Vec::new();
        for _ in 0..8 {
            // Find the delay by probing; it must be the first millisecond that is due.
            let mut waited = 0;
            while !refresh.retry_due(now + ms(waited)) {
                waited += 1;
                assert!(waited <= 2_000, "retry later than the cap");
            }
            delays.push(waited);
            now += ms(waited);
            assert!(!refresh.failed(now), "already paused");
        }
        assert_eq!(delays, [100, 200, 400, 800, 1600, 2000, 2000, 2000]);
    }

    #[test]
    fn a_failure_in_between_requests_still_lengthens_the_wait() {
        // An event-driven refresh can fail before the scheduled retry was ever asked for.
        let t0 = Instant::now();
        let mut refresh = Refresh::default();
        refresh.failed(t0);
        let t1 = t0 + ms(40);
        assert!(!refresh.failed(t1));
        assert!(!refresh.retry_due(t1 + ms(199)));
        assert!(refresh.retry_due(t1 + ms(200)));
    }

    #[test]
    fn an_applied_configuration_resumes_and_resets_the_backoff() {
        let t0 = Instant::now();
        let mut refresh = Refresh::default();
        assert!(!refresh.applied(), "applying while running is not a resume");
        refresh.failed(t0);
        refresh.failed(t0 + ms(100));
        refresh.failed(t0 + ms(300));
        assert!(refresh.applied(), "a valid configuration resumes");
        assert!(!refresh.is_paused());
        assert!(!refresh.retry_due(t0 + ms(60_000)), "nothing to retry");
        assert!(!refresh.applied());
        // A later pause starts over at the shortest delay.
        let t1 = t0 + ms(10_000);
        assert!(refresh.failed(t1));
        assert!(!refresh.retry_due(t1 + ms(99)));
        assert!(refresh.retry_due(t1 + ms(100)));
    }

    #[test]
    fn a_failed_retry_keeps_the_worker_paused_until_one_is_applied() {
        // The loop reports a configuration it cannot apply as a failure (see
        // `wayland::tests::a_configuration_whose_keymap_does_not_compile_keeps_the_worker_paused`
        // for the real validation), so it feeds `failed`, never `applied`.
        let t0 = Instant::now();
        let mut refresh = Refresh::default();
        refresh.failed(t0);
        assert!(refresh.retry_due(t0 + ms(100)));
        assert!(!refresh.failed(t0 + ms(110)));
        assert!(refresh.is_paused());
        assert!(refresh.retry_due(t0 + ms(310)));
        assert!(refresh.applied());
        assert!(!refresh.is_paused());
    }

    #[test]
    fn configuration_events_invalidate_snapshots_and_mark_the_watcher_dirty() {
        for name in [
            "activelayout",
            "configreloaded",
            "monitoradded",
            "monitoraddedv2",
            "monitorremoved",
        ] {
            let (dirty, epoch) = (AtomicBool::new(false), AtomicU64::new(7));
            // A read that began before the event.
            let stamp = epoch.load(Ordering::Acquire);
            assert!(snapshot_is_current(stamp, &epoch));
            note_event(&IpcEvent::Event { name, data: "" }, &dirty, &epoch);
            assert!(
                dirty.load(Ordering::Acquire),
                "{name} marks the watcher dirty"
            );
            assert!(
                !snapshot_is_current(stamp, &epoch),
                "{name} must invalidate a snapshot read before it"
            );
            // A read that began after it is current again.
            assert!(snapshot_is_current(epoch.load(Ordering::Acquire), &epoch));
        }
        // The event socket coming back may have lost events.
        let (dirty, epoch) = (AtomicBool::new(false), AtomicU64::new(0));
        note_event(&IpcEvent::Connected, &dirty, &epoch);
        assert!(dirty.load(Ordering::Acquire) && !snapshot_is_current(0, &epoch));
    }

    #[test]
    fn unrelated_events_leave_snapshots_current() {
        let (dirty, epoch) = (AtomicBool::new(false), AtomicU64::new(3));
        for event in [
            IpcEvent::Event {
                name: "workspace",
                data: "2",
            },
            IpcEvent::Event {
                name: "activewindow",
                data: "",
            },
            IpcEvent::Disconnected,
        ] {
            note_event(&event, &dirty, &epoch);
        }
        assert!(!dirty.load(Ordering::Acquire));
        assert!(snapshot_is_current(3, &epoch));
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
