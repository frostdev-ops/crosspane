//! Metadata-only pasteboard observation and admitted text reads (WP-C3a, CLIP-v0 §5).
//! Native objects stay on the main thread. `on_main` bounds scheduling, but cannot preempt
//! a native pasteboard call already executing there. Content is never cached or formatted.
//! Promise/provider methods and the `ClipboardHost` implementation arrive in WP-C3b.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crosspane_platform::{ClipKinds, ClipboardEvent, EventSink, IoGate, PlatformError};
use crosspane_types::ClipKind;
use objc2::rc::{Retained, autoreleasepool};
use objc2_app_kit::NSPasteboard;
use objc2_foundation::{NSString, NSUTF8StringEncoding, NSUUID};
use zeroize::Zeroizing;

use crate::main_thread::on_main;

const POLL: Duration = Duration::from_millis(250);
const MAIN_WAIT: Duration = Duration::from_millis(500);
const TEXT: &str = "public.utf8-plain-text";
const LEGACY_TEXT: &str = "NSStringPboardType";
const PNG: &str = "public.png";
const MARKER: &str = "io.frostdev.crosspane.promise.";

/// Production uses `General`; tests must use a uniquely named `Private` pasteboard.
#[derive(Clone, Debug)]
pub enum PasteboardName {
    General,
    Private(String),
}

impl PasteboardName {
    // Called only inside on_main; no retained AppKit object crosses threads.
    fn board(&self) -> Retained<NSPasteboard> {
        match self {
            Self::General => NSPasteboard::generalPasteboard(),
            Self::Private(name) => NSPasteboard::pasteboardWithName(&NSString::from_str(name)),
        }
    }
}

/// Pasteboard access metadata, never an inference about permission or read readiness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessBehavior {
    Default,
    Ask,
    AlwaysAllow,
    AlwaysDeny,
    Unknown(i64),
}

impl From<i64> for AccessBehavior {
    fn from(raw: i64) -> Self {
        match raw {
            0 => Self::Default,
            1 => Self::Ask,
            2 => Self::AlwaysAllow,
            3 => Self::AlwaysDeny,
            other => Self::Unknown(other),
        }
    }
}

#[derive(Clone, Copy)]
struct Snapshot {
    count: isize,
    kinds: ClipKinds,
    marker: bool,
    access: AccessBehavior,
}

struct State {
    observed: Snapshot,
    // WP-C3b records the successful write's offer and changeCount here.
    own: Option<(u64, isize)>,
    sink: Option<Arc<dyn EventSink<ClipboardEvent>>>,
}

impl State {
    fn observe(&mut self, next: Snapshot) -> Vec<ClipboardEvent> {
        let mut events = Vec::new();
        if self.observed.count != next.count {
            let ours = self
                .own
                .is_some_and(|(_, count)| count == next.count && next.marker);
            if !ours {
                if let Some((offer, _)) = self.own.take() {
                    events.push(ClipboardEvent::PromiseLost { offer });
                }
                events.push(ClipboardEvent::Changed { kinds: next.kinds });
            }
        }
        self.observed = next;
        events
    }
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    alive: AtomicBool,
}

/// A sendable watch/read façade. WP-C3b adds the full frozen `ClipboardHost` implementation.
pub struct MacClipboard {
    gate: Arc<IoGate>,
    name: PasteboardName,
    marker: String,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for MacClipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacClipboard").finish_non_exhaustive()
    }
}

impl MacClipboard {
    pub fn new(gate: Arc<IoGate>, pasteboard: PasteboardName) -> Result<Self, PlatformError> {
        let name = pasteboard.clone();
        let (marker, observed) = on_main(MAIN_WAIT, move |_| {
            autoreleasepool(|_| {
                let marker = format!(
                    "{MARKER}{}",
                    NSUUID::UUID()
                        .UUIDString()
                        .to_string()
                        .replace('-', "")
                        .to_ascii_lowercase()
                );
                let observed = snapshot(&name.board(), &marker, None);
                (marker, observed)
            })
        })?;
        Ok(Self {
            gate,
            name: pasteboard,
            marker,
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    observed,
                    own: None,
                    sink: None,
                }),
                wake: Condvar::new(),
                alive: AtomicBool::new(true),
            }),
            worker: None,
        })
    }

    /// Deliver the initial metadata and then ordered changes from the polling worker.
    pub fn subscribe(
        &mut self,
        sink: Arc<dyn EventSink<ClipboardEvent>>,
    ) -> Result<(), PlatformError> {
        let mut state = self.shared.state.lock().map_err(|_| poisoned())?;
        if state.sink.is_some() {
            return Err(PlatformError::Backend(
                "clipboard already subscribed".into(),
            ));
        }
        let shared = Arc::clone(&self.shared);
        let name = self.name.clone();
        let marker = self.marker.clone();
        let worker = thread::Builder::new()
            .name("crosspane-clipboard".into())
            .spawn(move || watch(shared, name, marker))
            .map_err(|_| PlatformError::Backend("clipboard watcher did not start".into()))?;
        state.sink = Some(sink);
        self.worker = Some(worker);
        Ok(())
    }

    /// Cached kinds from the last successful metadata observation; never reads content.
    pub fn kinds(&self) -> Result<ClipKinds, PlatformError> {
        Ok(self
            .shared
            .state
            .lock()
            .map_err(|_| poisoned())?
            .observed
            .kinds)
    }

    /// Cached access metadata, refreshed every poll. Failed polls leave the last observation.
    /// Before macOS 15.4, which lacks this API, the value is `Unknown(-1)`.
    pub fn access_behavior(&self) -> AccessBehavior {
        self.shared
            .state
            .lock()
            .map_or(AccessBehavior::Unknown(-1), |s| s.observed.access)
    }

    /// An admitted text read. Image content is deferred to C5; PNG kind metadata is observed.
    pub fn read(&mut self, kind: ClipKind, max_bytes: usize) -> Result<Vec<u8>, PlatformError> {
        let epoch = self.gate.epoch();
        check_read(&self.gate, epoch, Instant::now() + MAIN_WAIT)?;
        if kind != ClipKind::Text {
            return Err(PlatformError::NotFound);
        }
        let gate = Arc::clone(&self.gate);
        let shared = Arc::clone(&self.shared);
        let name = self.name.clone();
        let marker = self.marker.clone();
        let deadline = Instant::now() + MAIN_WAIT;
        let result = on_main(MAIN_WAIT, move |_| {
            autoreleasepool(|_| {
                check_read(&gate, epoch, deadline)?;
                if !shared.alive.load(Ordering::Acquire) {
                    return Err(PlatformError::NotFound);
                }
                let board = name.board();
                let current = snapshot(&board, &marker, None);
                admit_text(current)?;
                check_read(&gate, epoch, deadline)?;
                let text = match board.stringForType(&NSString::from_str(TEXT)) {
                    Some(text) => text,
                    None => {
                        check_read(&gate, epoch, deadline)?;
                        board
                            .stringForType(&NSString::from_str(LEGACY_TEXT))
                            .ok_or(PlatformError::NotFound)?
                    }
                };
                let bytes = text
                    .dataUsingEncoding_allowLossyConversion(NSUTF8StringEncoding, false)
                    .ok_or(PlatformError::NotFound)?
                    .to_vec();
                check_read(&gate, epoch, deadline)?;
                normalize_text(bytes, max_bytes)
            })
        });
        self.deliver_read(epoch, result.and_then(|result| result))
    }

    fn deliver_read(
        &self,
        epoch: u64,
        result: Result<Vec<u8>, PlatformError>,
    ) -> Result<Vec<u8>, PlatformError> {
        let bytes = result.map(Zeroizing::new);
        if !self.gate.is_open() || self.gate.epoch() != epoch {
            return Err(PlatformError::Locked);
        }
        bytes.map(|mut bytes| std::mem::take(&mut *bytes))
    }
}

impl Drop for MacClipboard {
    fn drop(&mut self) {
        self.shared.alive.store(false, Ordering::Release);
        // Use the wait mutex so the stop notification cannot be lost before the worker sleeps.
        if let Ok(_state) = self.shared.state.lock() {
            self.shared.wake.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        // Never clear, restore, or otherwise change the native pasteboard on drop.
    }
}

fn watch(shared: Arc<Shared>, name: PasteboardName, marker: String) {
    let Ok(state) = shared.state.lock() else {
        return;
    };
    let initial = state.observed.kinds;
    let sink = state.sink.clone();
    drop(state);
    if shared.alive.load(Ordering::Acquire)
        && let Some(sink) = sink
    {
        sink.send(ClipboardEvent::Changed { kinds: initial });
    }
    let mut next_poll = Instant::now() + POLL;
    loop {
        let Ok(state) = shared.state.lock() else {
            return;
        };
        if !shared.alive.load(Ordering::Acquire) {
            return;
        }
        let Ok((state, _)) = shared
            .wake
            .wait_timeout(state, next_poll.saturating_duration_since(Instant::now()))
        else {
            return;
        };
        if !shared.alive.load(Ordering::Acquire) {
            return;
        }
        next_poll = Instant::now() + POLL;
        let previous = state.observed;
        drop(state);
        let alive = Arc::clone(&shared);
        let board_name = name.clone();
        let marker_type = marker.clone();
        let next = on_main(MAIN_WAIT, move |_| {
            autoreleasepool(|_| {
                if !alive.alive.load(Ordering::Acquire) {
                    return None;
                }
                Some(snapshot(&board_name.board(), &marker_type, Some(previous)))
            })
        });
        if let Ok(Some(next)) = next {
            let Ok(mut state) = shared.state.lock() else {
                return;
            };
            if !shared.alive.load(Ordering::Acquire) {
                return;
            }
            let events = state.observe(next);
            let sink = state.sink.clone();
            drop(state);
            if let Some(sink) = sink {
                for event in events {
                    sink.send(event);
                }
            }
        }
    }
}

fn snapshot(board: &NSPasteboard, marker: &str, previous: Option<Snapshot>) -> Snapshot {
    let count = board.changeCount();
    let access = if objc2::available!(macos = 15.4) {
        AccessBehavior::from(board.accessBehavior().0 as i64)
    } else {
        AccessBehavior::Unknown(-1)
    };
    if let Some(previous) = previous.filter(|s| s.count == count) {
        return Snapshot { access, ..previous };
    }
    let mut kinds = ClipKinds::default();
    let mut own_marker = false;
    if let Some(types) = board.types() {
        for index in 0..types.count() {
            let name = types.objectAtIndex(index).to_string();
            own_marker |= add_type(&mut kinds, &name, marker);
        }
    }
    Snapshot {
        count,
        kinds,
        marker: own_marker,
        access,
    }
}

fn add_type(kinds: &mut ClipKinds, name: &str, marker: &str) -> bool {
    kinds.text |= matches!(name, TEXT | LEGACY_TEXT);
    kinds.image |= name == PNG;
    name == marker
}

fn admit_text(snapshot: Snapshot) -> Result<(), PlatformError> {
    if snapshot.marker || !snapshot.kinds.text {
        return Err(PlatformError::NotFound);
    }
    Ok(())
}

fn check_read(gate: &IoGate, epoch: u64, deadline: Instant) -> Result<(), PlatformError> {
    if !gate.is_open() || gate.epoch() != epoch {
        return Err(PlatformError::Locked);
    }
    if Instant::now() >= deadline {
        return Err(PlatformError::Timeout);
    }
    Ok(())
}

fn normalize_text(bytes: Vec<u8>, max_bytes: usize) -> Result<Vec<u8>, PlatformError> {
    let text = String::from_utf8(bytes).map_err(|_| PlatformError::NotFound)?;
    let bytes = text.replace("\r\n", "\n").into_bytes();
    if bytes.is_empty() {
        return Err(PlatformError::NotFound);
    }
    if bytes.len() > max_bytes {
        return Err(PlatformError::TooLarge);
    }
    Ok(bytes)
}

fn poisoned() -> PlatformError {
    PlatformError::Backend("clipboard state unavailable".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn snapshot_fixture(count: isize, marker: bool) -> Snapshot {
        Snapshot {
            count,
            marker,
            kinds: ClipKinds {
                text: true,
                image: false,
            },
            access: AccessBehavior::AlwaysAllow,
        }
    }

    fn state() -> State {
        State {
            observed: snapshot_fixture(1, false),
            own: None,
            sink: None,
        }
    }

    // No native constructor, run loop or pasteboard is used by headless unit tests.
    fn fixture(gate: Arc<IoGate>) -> MacClipboard {
        MacClipboard {
            gate,
            name: PasteboardName::Private("io.frostdev.crosspane.c3a.unit-unused".into()),
            marker: format!("{MARKER}00000000000000000000000000000000"),
            shared: Arc::new(Shared {
                state: Mutex::new(state()),
                wake: Condvar::new(),
                alive: AtomicBool::new(true),
            }),
            worker: None,
        }
    }

    #[test]
    fn types_map_only_text_aliases_and_png() {
        for name in [TEXT, LEGACY_TEXT] {
            let mut kinds = ClipKinds::default();
            add_type(&mut kinds, name, MARKER);
            assert_eq!(
                kinds,
                ClipKinds {
                    text: true,
                    image: false
                }
            );
        }
        let mut kinds = ClipKinds::default();
        for name in [PNG, "public.tiff", "public.html", MARKER] {
            add_type(&mut kinds, name, MARKER);
        }
        assert_eq!(
            kinds,
            ClipKinds {
                text: false,
                image: true
            }
        );
    }

    #[test]
    fn only_this_instances_exact_marker_is_recognized() {
        let ours = format!("{MARKER}00000000000000000000000000000001");
        let others = format!("{MARKER}00000000000000000000000000000002");
        let mut kinds = ClipKinds::default();
        assert!(add_type(&mut kinds, &ours, &ours));
        assert!(!add_type(&mut kinds, &others, &ours));
        assert!(!add_type(&mut kinds, MARKER, &ours));
        assert_eq!(kinds, ClipKinds::default());
    }

    #[test]
    fn own_promise_or_missing_text_returns_not_found_before_content_read() {
        assert!(matches!(
            admit_text(snapshot_fixture(1, true)),
            Err(PlatformError::NotFound)
        ));
        let mut absent = snapshot_fixture(1, false);
        absent.kinds.text = false;
        assert!(matches!(admit_text(absent), Err(PlatformError::NotFound)));
        assert!(admit_text(snapshot_fixture(1, false)).is_ok());
    }

    #[test]
    fn own_marker_and_written_count_suppress_changed() {
        let mut state = state();
        state.own = Some((7, 2));
        assert!(state.observe(snapshot_fixture(2, true)).is_empty());
        assert!(state.observe(snapshot_fixture(2, true)).is_empty());
        assert_eq!(state.own, Some((7, 2)));
    }

    #[test]
    fn marker_without_written_count_does_not_claim_ownership() {
        let mut state = state();
        assert_eq!(
            state.observe(snapshot_fixture(2, true)),
            vec![ClipboardEvent::Changed {
                kinds: state.observed.kinds
            }]
        );
    }

    #[test]
    fn change_away_emits_promise_lost_once_then_changed() {
        let mut state = state();
        state.own = Some((7, 1));
        assert_eq!(
            state.observe(snapshot_fixture(2, false)),
            vec![
                ClipboardEvent::PromiseLost { offer: 7 },
                ClipboardEvent::Changed {
                    kinds: state.observed.kinds
                },
            ]
        );
        assert!(state.observe(snapshot_fixture(2, false)).is_empty());
        assert_eq!(state.observe(snapshot_fixture(3, false)).len(), 1);
    }

    #[test]
    fn matching_count_without_marker_is_not_our_promise() {
        let mut state = state();
        state.own = Some((7, 2));
        assert!(matches!(
            state.observe(snapshot_fixture(2, false)).first(),
            Some(ClipboardEvent::PromiseLost { offer: 7 })
        ));
    }

    #[test]
    fn text_normalizes_crlf_preserves_bare_cr_and_utf8() {
        assert_eq!(
            normalize_text("a\r\nb\rc\n雪".as_bytes().to_vec(), 20).unwrap(),
            "a\nb\rc\n雪".as_bytes()
        );
    }

    #[test]
    fn normalized_byte_limit_is_exact_and_never_truncates() {
        assert_eq!(normalize_text(b"a\r\n".to_vec(), 2).unwrap(), b"a\n");
        assert!(matches!(
            normalize_text("雪".as_bytes().to_vec(), 2),
            Err(PlatformError::TooLarge)
        ));
        assert_eq!(
            normalize_text("雪".as_bytes().to_vec(), 3).unwrap(),
            "雪".as_bytes()
        );
    }

    #[test]
    fn empty_and_invalid_utf8_are_not_found() {
        for bytes in [Vec::new(), vec![0xff]] {
            assert!(matches!(
                normalize_text(bytes, 10),
                Err(PlatformError::NotFound)
            ));
        }
    }

    #[test]
    fn gate_closed_read_returns_locked_without_native_dispatch() {
        let mut host = fixture(IoGate::new());
        assert!(matches!(
            host.read(ClipKind::Text, 10),
            Err(PlatformError::Locked)
        ));
        assert!(matches!(
            host.read(ClipKind::Image, 10),
            Err(PlatformError::Locked)
        ));
    }

    #[test]
    fn close_reopen_between_admission_and_execution_is_locked() {
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        let epoch = gate.epoch();
        gate.set_session_permits(false);
        gate.set_session_permits(true);
        assert!(matches!(
            check_read(&gate, epoch, Instant::now() + MAIN_WAIT),
            Err(PlatformError::Locked)
        ));
    }

    #[test]
    fn close_reopen_between_task_completion_and_delivery_is_locked() {
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        let host = fixture(Arc::clone(&gate));
        let epoch = gate.epoch();
        // The dispatched task has completed; the caller has not delivered its result yet.
        let completed = Ok(normalize_text(b"c3a-owned-test\r\n".to_vec(), 100).unwrap());
        gate.set_session_permits(false);
        gate.set_session_permits(true);
        assert!(matches!(
            host.deliver_read(epoch, completed),
            Err(PlatformError::Locked)
        ));
    }

    #[test]
    fn gate_close_between_task_completion_and_delivery_is_locked() {
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        let host = fixture(Arc::clone(&gate));
        let epoch = gate.epoch();
        let completed = Ok(b"c3a-owned-test".to_vec());
        gate.set_engine_permits(false);
        assert!(matches!(
            host.deliver_read(epoch, completed),
            Err(PlatformError::Locked)
        ));
    }

    #[test]
    fn unchanged_open_gate_delivers_completed_bytes() {
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        let host = fixture(Arc::clone(&gate));
        assert_eq!(
            host.deliver_read(gate.epoch(), Ok(b"c3a-owned-test".to_vec()))
                .unwrap(),
            b"c3a-owned-test"
        );
    }

    #[test]
    fn expired_read_is_timeout_without_native_dispatch() {
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        assert!(matches!(
            check_read(&gate, gate.epoch(), Instant::now()),
            Err(PlatformError::Timeout)
        ));
    }

    #[test]
    fn image_content_is_deferred_but_png_metadata_is_retained() {
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        let mut host = fixture(gate);
        host.shared.state.lock().unwrap().observed.kinds.image = true;
        assert!(host.kinds().unwrap().image);
        assert!(matches!(
            host.read(ClipKind::Image, 10),
            Err(PlatformError::NotFound)
        ));
    }

    #[test]
    fn access_metadata_keeps_unknown_values_and_last_observation() {
        assert_eq!(
            [0, 1, 2, 3, 42].map(AccessBehavior::from),
            [
                AccessBehavior::Default,
                AccessBehavior::Ask,
                AccessBehavior::AlwaysAllow,
                AccessBehavior::AlwaysDeny,
                AccessBehavior::Unknown(42)
            ]
        );
        let host = fixture(IoGate::new());
        assert_eq!(host.access_behavior(), AccessBehavior::AlwaysAllow);
        // Same native changeCount can still carry fresh access metadata, without Changed.
        let mut state = host.shared.state.lock().unwrap();
        let mut next = state.observed;
        next.access = AccessBehavior::Ask;
        assert!(state.observe(next).is_empty());
        drop(state);
        assert_eq!(host.access_behavior(), AccessBehavior::Ask);
    }

    #[test]
    fn debug_has_no_clipboard_data_or_pasteboard_name() {
        assert_eq!(
            format!("{:?}", fixture(IoGate::new())),
            "MacClipboard { .. }"
        );
    }

    #[test]
    fn drop_wakes_and_joins_watcher_without_native_calls() {
        let mut host = fixture(IoGate::new());
        let shared = Arc::clone(&host.shared);
        let (tx, rx) = mpsc::channel();
        host.worker = Some(thread::spawn(move || {
            let state = shared.state.lock().unwrap();
            tx.send(()).unwrap();
            let _state = shared
                .wake
                .wait_while(state, |_| shared.alive.load(Ordering::Acquire))
                .unwrap();
        }));
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let shared = Arc::clone(&host.shared);
        drop(host);
        assert!(!shared.alive.load(Ordering::Acquire));
    }
}
