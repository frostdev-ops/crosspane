//! Metadata-only pasteboard observation and admitted text reads (WP-C3a, CLIP-v0 §5).
//! Native objects stay on the main thread. `on_main` bounds scheduling, but cannot preempt
//! a native pasteboard call already executing there. Read content is never cached or formatted.
//! Lazy promises retain native items/providers on the main thread until AppKit releases them.
//! Withdrawal disables unsupplied representations and cancels pending pastes without clearing
//! the pasteboard. Already supplied data may remain locally until the next copy; revocation
//! stops future transfer rather than recalling bytes delivered under a valid grant.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crosspane_platform::{
    ClipKinds, ClipboardEvent, ClipboardHost, EventSink, IoGate, LocalPasteId, PlatformError,
};
use crosspane_types::ClipKind;
use objc2::rc::{Retained, autoreleasepool};
use objc2_app_kit::NSPasteboard;
use objc2_foundation::{NSString, NSUTF8StringEncoding, NSUUID};
use zeroize::Zeroizing;

use crate::main_thread::on_main;

mod paste;
mod provider;

const POLL: Duration = Duration::from_millis(250);
const MAIN_WAIT: Duration = Duration::from_millis(500);
const CONSTRUCTION_WAIT: Duration = Duration::from_secs(10);
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
    fn installed(&mut self, owner: (u64, isize), next: Snapshot, after: isize) -> bool {
        if owner.1 != next.count || after != next.count || !next.marker {
            return false;
        }
        self.own = Some(owner);
        self.observed = next;
        true
    }

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
    pastes: Mutex<paste::Pastes>,
    wake: Condvar,
    alive: AtomicBool,
}

impl Shared {
    fn begin_paste(&self, epoch: u64) -> Option<LocalPasteId> {
        let mut pastes = self.pastes.lock().ok()?;
        if !self.alive.load(Ordering::Acquire) {
            return None;
        }
        pastes.begin(Instant::now(), epoch)
    }

    fn poll_paste(&self, id: LocalPasteId, gate: &IoGate) -> Option<Option<Zeroizing<Vec<u8>>>> {
        self.pastes.lock().ok()?.poll(
            id,
            Instant::now(),
            gate.epoch(),
            gate.is_open() && self.alive.load(Ordering::Acquire),
        )
    }

    fn prepare_withdraw(&self, offer: u64) -> Result<Option<(u64, isize)>, PlatformError> {
        let state = self.state.lock().map_err(|_| poisoned())?;
        let owner = state.own.filter(|owner| owner.0 == offer);
        if owner.is_some() {
            self.cancel();
        }
        Ok(owner)
    }

    fn cancel(&self) {
        if let Ok(mut pastes) = self.pastes.lock() {
            pastes.cancel();
        }
    }

    fn lost(&self, owner: (u64, isize), notify: bool) {
        if let Ok(mut state) = self.state.lock()
            && state.own == Some(owner)
        {
            state.own = None;
            self.cancel();
            if notify
                && self.alive.load(Ordering::Acquire)
                && let Some(sink) = &state.sink
            {
                sink.send(ClipboardEvent::PromiseLost { offer: owner.0 });
            }
        }
    }
}

/// A sendable clipboard façade; retained native objects never cross threads.
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
    /// Wait up to 10 s for AppKit startup off-main; steady-state I/O uses 500 ms.
    /// The main thread must run its loop, or call this constructor directly.
    pub fn new(gate: Arc<IoGate>, pasteboard: PasteboardName) -> Result<Self, PlatformError> {
        let name = pasteboard.clone();
        let (marker, observed) = on_main(CONSTRUCTION_WAIT, move |_| {
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
                pastes: Mutex::new(paste::Pastes::default()),
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

impl ClipboardHost for MacClipboard {
    fn subscribe(&mut self, sink: Arc<dyn EventSink<ClipboardEvent>>) -> Result<(), PlatformError> {
        Self::subscribe(self, sink)
    }
    fn kinds(&self) -> Result<ClipKinds, PlatformError> {
        Self::kinds(self)
    }
    fn read(&mut self, kind: ClipKind, max_bytes: usize) -> Result<Vec<u8>, PlatformError> {
        Self::read(self, kind, max_bytes)
    }

    fn promise(&mut self, offer: u64, kinds: ClipKinds) -> Result<(), PlatformError> {
        let epoch = self.gate.epoch();
        let deadline = Instant::now() + MAIN_WAIT;
        check_read(&self.gate, epoch, deadline)?;
        if !kinds.text && !kinds.image {
            return Err(PlatformError::Backend("empty clipboard promise".into()));
        }
        let context = provider::Context::new(self, offer, kinds);
        let task = Arc::clone(&context);
        let result = provider::on_default(move |_| task.install(epoch, deadline)).and_then(|r| r);
        if result.is_err() {
            context.abandoned.store(true, Ordering::Release);
        }
        deliver_install(&self.gate, epoch, result, || context.rollback())
    }

    fn fulfil(&mut self, paste: LocalPasteId, data: Option<Vec<u8>>) {
        let data = data.map(Zeroizing::new);
        if let Ok(mut pastes) = self.shared.pastes.lock() {
            pastes.answer(
                paste,
                data,
                Instant::now(),
                self.gate.epoch(),
                self.gate.is_open() && self.shared.alive.load(Ordering::Acquire),
            );
        }
    }

    fn withdraw(&mut self, offer: u64) -> Result<(), PlatformError> {
        let Some(owner) = self.shared.prepare_withdraw(offer)? else {
            return Ok(());
        };
        provider::withdraw(self.marker.clone(), Arc::clone(&self.shared), owner)
    }
}

impl Drop for MacClipboard {
    fn drop(&mut self) {
        self.shared.alive.store(false, Ordering::Release);
        self.shared.cancel();
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
    if shared.alive.load(Ordering::Acquire)
        && state.own.is_none()
        && let Some(sink) = &state.sink
    {
        sink.send(ClipboardEvent::Changed {
            kinds: state.observed.kinds,
        });
    }
    drop(state);
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
        let _ = on_main(MAIN_WAIT, move |_| {
            autoreleasepool(|_| {
                if !alive.alive.load(Ordering::Acquire) {
                    return;
                }
                // Observe and publish on main, so promise writes cannot race the snapshot.
                let next = snapshot(&board_name.board(), &marker_type, Some(previous));
                if let Ok(mut state) = alive.state.lock()
                    && alive.alive.load(Ordering::Acquire)
                {
                    let events = state.observe(next);
                    if events
                        .iter()
                        .any(|event| matches!(event, ClipboardEvent::PromiseLost { .. }))
                    {
                        alive.cancel();
                    }
                    if let Some(sink) = &state.sink {
                        for event in events {
                            sink.send(event);
                        }
                    }
                }
            })
        });
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

fn deliver_install(
    gate: &IoGate,
    epoch: u64,
    result: Result<(), PlatformError>,
    cleanup: impl FnOnce(),
) -> Result<(), PlatformError> {
    if !gate.is_open() || gate.epoch() != epoch {
        cleanup();
        return Err(PlatformError::Locked);
    }
    if result.is_err() {
        cleanup();
    }
    result
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
    pub(super) fn fixture(gate: Arc<IoGate>) -> MacClipboard {
        MacClipboard {
            gate,
            name: PasteboardName::Private("io.frostdev.crosspane.c3a.unit-unused".into()),
            marker: format!("{MARKER}00000000000000000000000000000000"),
            shared: Arc::new(Shared {
                state: Mutex::new(state()),
                pastes: Mutex::new(paste::Pastes::default()),
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

    pub(super) fn open_gate() -> Arc<IoGate> {
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        gate
    }

    #[test]
    fn full_trait_is_send_and_empty_promise_mapping_is_exact() {
        fn frozen_host<T: ClipboardHost + Send>() {}
        frozen_host::<MacClipboard>();
        let mut host = fixture(open_gate());
        assert!(matches!(host.promise(1, ClipKinds::default()),
            Err(PlatformError::Backend(message)) if message == "empty clipboard promise"));
        host.gate.set_engine_permits(false);
        assert!(matches!(
            host.promise(1, ClipKinds::default()),
            Err(PlatformError::Locked)
        ));
    }

    #[test]
    fn lost_notification_matches_written_generation_and_is_delivered_once() {
        let host = fixture(open_gate());
        let (tx, rx) = mpsc::channel();
        {
            let mut state = host.shared.state.lock().unwrap();
            state.own = Some((7, 11));
            state.sink = Some(Arc::new(move |event| {
                tx.send(event).unwrap();
            }));
        }
        host.shared.lost((7, 10), true);
        assert!(rx.try_recv().is_err());
        host.shared.lost((7, 11), true);
        host.shared.lost((7, 11), true);
        assert_eq!(
            rx.try_recv().unwrap(),
            ClipboardEvent::PromiseLost { offer: 7 }
        );
        assert!(rx.try_recv().is_err());
        assert!(host.shared.state.lock().unwrap().own.is_none());
    }

    #[test]
    fn fulfil_from_another_thread_is_consumable_once() {
        let mut host = fixture(open_gate());
        let shared = Arc::clone(&host.shared);
        let epoch = host.gate.epoch();
        let id = shared
            .pastes
            .lock()
            .unwrap()
            .begin(Instant::now(), epoch)
            .unwrap();
        let _host = thread::spawn(move || {
            host.fulfil(id, Some(b"private unit fixture".to_vec()));
            host.fulfil(id, Some(b"duplicate unit fixture".to_vec()));
            host
        })
        .join()
        .unwrap();
        let bytes = shared
            .pastes
            .lock()
            .unwrap()
            .poll(id, Instant::now(), epoch, true)
            .unwrap()
            .unwrap();
        assert_eq!(&*bytes, b"private unit fixture");
        assert_eq!(
            shared
                .pastes
                .lock()
                .unwrap()
                .poll(id, Instant::now(), epoch, true),
            Some(None)
        );
    }

    #[test]
    fn lost_and_drop_cancel_ready_pastes_without_resurrecting_native_content() {
        for drop_host in [false, true] {
            let mut host = fixture(open_gate());
            let shared = Arc::clone(&host.shared);
            let epoch = host.gate.epoch();
            let id = shared
                .pastes
                .lock()
                .unwrap()
                .begin(Instant::now(), epoch)
                .unwrap();
            host.shared.state.lock().unwrap().own = Some((9, 3));
            host.fulfil(id, Some(b"cancelled unit fixture".to_vec()));
            if drop_host {
                drop(host);
            } else {
                shared.lost((9, 3), true);
            }
            assert_eq!(
                shared
                    .pastes
                    .lock()
                    .unwrap()
                    .poll(id, Instant::now(), epoch, true),
                Some(None)
            );
        }
    }

    #[test]
    fn fulfil_after_gate_close_reopen_or_for_unknown_id_is_empty() {
        let mut host = fixture(open_gate());
        let epoch = host.gate.epoch();
        let id = host
            .shared
            .pastes
            .lock()
            .unwrap()
            .begin(Instant::now(), epoch)
            .unwrap();
        host.fulfil(
            LocalPasteId(id.0 + 1),
            Some(b"unknown unit fixture".to_vec()),
        );
        host.gate.set_engine_permits(false);
        host.gate.set_engine_permits(true);
        host.fulfil(id, Some(b"stale unit fixture".to_vec()));
        assert_eq!(
            host.shared
                .pastes
                .lock()
                .unwrap()
                .poll(id, Instant::now(), host.gate.epoch(), true),
            Some(None)
        );
    }

    #[test]
    fn external_copy_between_write_and_snapshot_never_publishes_ownership() {
        let mut state = state();
        let written = (7, 101); // clearContents returned THIS write's generation.
        let copied = snapshot_fixture(102, false); // Another app copied after writeObjects.
        assert!(!state.installed(written, copied, 102));
        assert!(state.own.is_none());
        assert_eq!(
            state.observed.count, 1,
            "rejected metadata must remain observable to watch"
        );
    }

    #[test]
    fn own_marker_on_a_different_generation_is_not_our_write() {
        let mut state = state();
        assert!(!state.installed((7, 101), snapshot_fixture(102, true), 102));
        assert!(state.own.is_none());
    }

    #[test]
    fn copy_during_metadata_snapshot_prevents_ownership_publication() {
        let mut state = state();
        assert!(!state.installed((7, 101), snapshot_fixture(101, true), 102));
        assert!(state.own.is_none());
    }

    #[test]
    fn matching_withdraw_cancels_rust_slot_before_native_work_is_awaited() {
        let host = fixture(open_gate());
        host.shared.state.lock().unwrap().own = Some((7, 101));
        let id = host.shared.begin_paste(host.gate.epoch()).unwrap();
        assert_eq!(host.shared.prepare_withdraw(7).unwrap(), Some((7, 101)));
        assert_eq!(host.shared.poll_paste(id, &host.gate), Some(None));
    }

    #[test]
    fn unrelated_withdraw_does_not_cancel_the_current_paste() {
        let host = fixture(open_gate());
        host.shared.state.lock().unwrap().own = Some((7, 101));
        let id = host.shared.begin_paste(host.gate.epoch()).unwrap();
        assert_eq!(host.shared.prepare_withdraw(8).unwrap(), None);
        assert!(host.shared.poll_paste(id, &host.gate).is_none());
    }

    #[test]
    fn drop_between_admission_and_slot_lock_cannot_allocate_a_paste() {
        let host = fixture(open_gate());
        let mut slots = host.shared.pastes.lock().unwrap();
        let shared = Arc::clone(&host.shared);
        let epoch = host.gate.epoch();
        let (ready, admitted) = mpsc::channel();
        let request = thread::spawn(move || {
            assert!(shared.alive.load(Ordering::Acquire));
            ready.send(()).unwrap();
            shared.begin_paste(epoch)
        });
        admitted.recv().unwrap();
        // Drop wins while request is blocked at the paste mutex; it cancels an empty slot.
        host.shared.alive.store(false, Ordering::Release);
        slots.cancel();
        drop(slots);
        assert!(request.join().unwrap().is_none());
    }

    #[test]
    fn polling_after_drop_answers_empty_even_before_cancellation_acquires_the_slot() {
        let host = fixture(open_gate());
        let id = host.shared.begin_paste(host.gate.epoch()).unwrap();
        host.shared.alive.store(false, Ordering::Release);
        assert_eq!(host.shared.poll_paste(id, &host.gate), Some(None));
    }

    #[test]
    fn gate_change_during_native_install_rolls_back_the_owned_generation() {
        for reopen in [false, true] {
            let host = fixture(open_gate());
            let epoch = host.gate.epoch();
            assert!(host.shared.state.lock().unwrap().installed(
                (7, 101),
                snapshot_fixture(101, true),
                101
            ));
            host.gate.set_engine_permits(false);
            if reopen {
                host.gate.set_engine_permits(true);
            }
            let cleaned = AtomicBool::new(false);
            let result = deliver_install(&host.gate, epoch, Ok(()), || {
                assert_eq!(host.shared.prepare_withdraw(7).unwrap(), Some((7, 101)));
                host.shared.lost((7, 101), true);
                cleaned.store(true, Ordering::Release);
            });
            assert!(matches!(result, Err(PlatformError::Locked)));
            assert!(cleaned.load(Ordering::Acquire));
            assert!(host.shared.state.lock().unwrap().own.is_none());
        }
    }

    #[test]
    fn close_reopen_after_install_completion_before_delivery_is_locked_and_cleans_up() {
        let host = fixture(open_gate());
        let epoch = host.gate.epoch();
        host.shared.state.lock().unwrap().own = Some((7, 101));
        let completed = Ok(());
        host.gate.set_engine_permits(false);
        host.gate.set_engine_permits(true);
        let cleaned = AtomicBool::new(false);
        let result = deliver_install(&host.gate, epoch, completed, || {
            host.shared.lost((7, 101), true);
            cleaned.store(true, Ordering::Release);
        });
        assert!(matches!(result, Err(PlatformError::Locked)));
        assert!(cleaned.load(Ordering::Acquire));
        assert!(host.shared.state.lock().unwrap().own.is_none());
    }

    #[test]
    fn withdrawal_disables_unfulfilled_generation_without_false_loss_notice() {
        let mut host = fixture(open_gate());
        let (tx, rx) = mpsc::channel();
        {
            let mut state = host.shared.state.lock().unwrap();
            state.own = Some((7, 101));
            state.sink = Some(Arc::new(move |event| tx.send(event).unwrap()));
        }
        let id = host.shared.begin_paste(host.gate.epoch()).unwrap();
        host.shared.lost((7, 101), false); // Synchronous disable, before native release.
        host.fulfil(id, Some(b"revoked private fixture".to_vec()));
        assert!(host.shared.state.lock().unwrap().own.is_none());
        assert_eq!(host.shared.poll_paste(id, &host.gate), Some(None));
        assert!(
            rx.try_recv().is_err(),
            "withdrawal did not change the native generation"
        );
    }

    #[test]
    fn external_copy_after_validation_survives_generation_disable() {
        let host = fixture(open_gate());
        host.shared.state.lock().unwrap().own = Some((7, 101));
        let owner = host.shared.prepare_withdraw(7).unwrap().unwrap();
        // A copy after validation is independent of our Rust ownership/cache identity.
        let clipboard = Mutex::new((102, b"newer private local copy".to_vec()));
        host.shared.lost(owner, false);
        assert_eq!(
            *clipboard.lock().unwrap(),
            (102, b"newer private local copy".to_vec())
        );
        assert!(host.shared.state.lock().unwrap().own.is_none());
        // A late old disable must also preserve a newer promise's obligation.
        host.shared.state.lock().unwrap().own = Some((8, 103));
        host.shared.lost(owner, false);
        assert_eq!(host.shared.state.lock().unwrap().own, Some((8, 103)));
    }
}
