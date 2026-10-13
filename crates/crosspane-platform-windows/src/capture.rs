//! Windows capture: bounded LL callbacks, mouse-only Raw Input and positive keyboard probes.
//!
//! Keyboard Raw Input belongs exclusively to GlobalHotkeys. A pumping thread does not prove
//! either hook exists. While captured, one nonce-tagged F24 SCANCODE pair is outstanding; missing
//! acknowledgment ends capture within approximately 500 ms plus scheduling/native-call delay.
//! After hook loss at most one pair can reach the foreground. F24 is not universally unused.
//! Partial insertion owes only its matching up, retried three times; a failed up remains \[U\],
//! possibly held locally. It is never reported as verified cleanup. A probe is never sent to a
//! foreground that UIPI would silently drop it for (a higher-integrity or secure-desktop window,
//! or one whose integrity cannot be read): that ends an active capture as `Lost` ("elevated
//! foreground") and refuses an activation with `SecureInput`, and is never a fault. Native calls
//! have no public hard real-time guarantee. Mouse Raw Input mismatch conservatively ends both hooks without
//! claiming that Windows removes them together. This backend exclusively owns mouse Raw Input.

#![allow(unsafe_code)]

use std::{
    cell::Cell,
    collections::VecDeque,
    mem::size_of,
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use crate::inject::{own_integrity, process_integrity};
use crosspane_platform::{
    CaptureAbort, CaptureEvent, CaptureId, CapturePortal, CaptureStart, Edge, EndReason, EventSink,
    InputCapture, IoGate, MotionKind, PlatformError, PortalId,
};
use crosspane_types::{
    geom::{PixelRect, PointDevice},
    hid::MouseButton,
    id::DisplayId,
    input::LockKeys,
    time::MonoTime,
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONULL, MONITORINFO, MonitorFromPoint},
    Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom},
    System::{
        LibraryLoader::GetModuleHandleW,
        Threading::{
            GetCurrentProcessId, GetCurrentThread, GetCurrentThreadId, SetThreadPriority,
            THREAD_PRIORITY_HIGHEST,
        },
    },
    UI::{
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, GetKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
            KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MAPVK_VK_TO_VSC_EX, MapVirtualKeyW, SendInput,
            VK_CAPITAL, VK_NUMLOCK, VK_SCROLL,
        },
        Input::{
            GetRawInputData, GetRegisteredRawInputDevices, RAWINPUT, RAWINPUTDEVICE,
            RAWINPUTHEADER, RID_INPUT, RIDEV_INPUTSINK, RIDEV_REMOVE, RIM_TYPEMOUSE,
            RegisterRawInputDevices,
        },
        WindowsAndMessaging::*,
    },
};

use crate::model::{
    capture::{
        self as model, ForegroundClass, Probe, ProbeAction, ProbeDecision, ProbePhase, ProbeStage,
        Resource, Watchdog,
    },
    geometry::{DisplayIds, MonitorProbe, displays},
    hook::{
        DragSettlementResult, HookState, KeyIn, KeySnapshot, MouseIn, MouseKind, drag_up_transition,
    },
};

const RECORDS: usize = 2048;
const WAKE: u32 = WM_APP + 71;
const COMMAND_TIMEOUT: Duration = Duration::from_millis(700);
static OWNER: AtomicBool = AtomicBool::new(false);
thread_local! { static CALLBACK: Cell<*const Shared> = const { Cell::new(null()) }; }

fn error(message: &'static str) -> PlatformError {
    PlatformError::Backend(message.into())
}
/// Fault sites, indexed by `Shared::fault_cause`. Fixed labels only: no key, button or position.
const FAULT_CAUSES: [&str; 17] = [
    "unrecorded",
    "event queue full",
    "callback ring full",
    "owner wake failed",
    "pointer release failed",
    "event delivery panicked",
    "unmapped key",
    "raw input read failed",
    "absolute raw mouse input",
    "owner panicked",
    "probe thread unavailable",
    "watchdog unavailable",
    "drag observer lost",
    "mouse raw input registration changed",
    "pointer clip lost",
    "probe submission lost",
    "probe unacknowledged",
];
/// Why the latest capture ended as `Lost`, as an index into `LOSS_CAUSES`. One backend per process.
static LAST_LOSS: AtomicU8 = AtomicU8::new(0);
const LOSS_CAUSES: [&str; 10] = [
    "unrecorded",
    "fault",
    "keyboard probe unacknowledged",
    "session gate closed or epoch changed",
    "owner thread stalled",
    "mouse hook silent while raw input moved",
    "activation raced a gate or generation change",
    "probe due while the gate was closed",
    "owner thread ended",
    "elevated foreground: input cannot be verified",
];
/// `LOSS_CAUSES` index: the foreground is elevated or secure, so UIPI would drop the probe.
const LOSS_ELEVATED: u8 = 9;
/// The cause of the latest `Lost` capture end in this process (fixed text, never input data).
pub fn last_loss() -> &'static str {
    LOSS_CAUSES
        .get(usize::from(LAST_LOSS.load(Ordering::Acquire)))
        .copied()
        .unwrap_or("unrecorded")
}
fn note_loss(cause: u8) {
    LAST_LOSS.store(cause, Ordering::Release);
}
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}
fn now(start: Instant) -> u64 {
    start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

fn event_time() -> MonoTime {
    crate::clock::now()
}

#[derive(Clone, Copy)]
struct Record {
    kind: u64,
    data: u64,
    id: u64,
    captured: bool,
    token: u64,
}

/// One native owner thread produces/consumes this ring; abort only changes separate atomics.
struct Ring {
    write: AtomicUsize,
    read: AtomicUsize,
    kind: [AtomicU64; RECORDS],
    data: [AtomicU64; RECORDS],
    id: [AtomicU64; RECORDS],
    captured: [AtomicBool; RECORDS],
    token: [AtomicU64; RECORDS],
}

impl Ring {
    fn new() -> Self {
        Self {
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            kind: std::array::from_fn(|_| AtomicU64::new(0)),
            data: std::array::from_fn(|_| AtomicU64::new(0)),
            id: std::array::from_fn(|_| AtomicU64::new(0)),
            captured: std::array::from_fn(|_| AtomicBool::new(false)),
            token: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
    fn push(&self, r: Record) -> bool {
        let write = self.write.load(Ordering::Relaxed);
        if write.wrapping_sub(self.read.load(Ordering::Acquire)) >= RECORDS {
            return false;
        }
        let index = write % RECORDS;
        self.kind[index].store(r.kind, Ordering::Relaxed);
        self.data[index].store(r.data, Ordering::Relaxed);
        self.id[index].store(r.id, Ordering::Relaxed);
        self.captured[index].store(r.captured, Ordering::Relaxed);
        self.token[index].store(r.token, Ordering::Relaxed);
        self.write.store(write.wrapping_add(1), Ordering::Release);
        true
    }
    fn pop(&self) -> Option<Record> {
        let read = self.read.load(Ordering::Relaxed);
        if read == self.write.load(Ordering::Acquire) {
            return None;
        }
        let index = read % RECORDS;
        let r = Record {
            kind: self.kind[index].load(Ordering::Relaxed),
            data: self.data[index].load(Ordering::Relaxed),
            id: self.id[index].load(Ordering::Relaxed),
            captured: self.captured[index].load(Ordering::Relaxed),
            token: self.token[index].load(Ordering::Relaxed),
        };
        self.read.store(read.wrapping_add(1), Ordering::Release);
        Some(r)
    }
}

struct Shared {
    gate: Arc<IoGate>,
    start: Instant,
    generation: AtomicU64,
    active: AtomicU64,
    terminal: AtomicU64,
    terminal_token: AtomicU64,
    end_claim: AtomicBool,
    end_pending: AtomicBool,
    active_epoch: AtomicU64,
    warming: AtomicBool,
    reason: AtomicU8,
    shutdown: AtomicBool,
    fault: AtomicBool,
    /// The first fault's site (`FAULT_CAUSES` index), for the error text. Never input data.
    fault_cause: AtomicU8,
    thread: AtomicU32,
    pump: AtomicU64,
    mouse_hook_count: AtomicU64,
    raw_count: AtomicU64,
    keyboard: AtomicUsize,
    mouse: AtomicUsize,
    window: AtomicUsize,
    cursor: AtomicUsize,
    clipped: AtomicBool,
    cleanup_verified: AtomicBool,
    nonce: AtomicUsize,
    ack: AtomicU8,
    probe_at: AtomicU64,
    probe_owed: AtomicBool,
    /// Set by the probe thread when, during warm-up, the foreground turned out to be elevated or
    /// secure and no probe was sent. The owner then refuses the activation; reset by each warm-up.
    foreground_refused: AtomicBool,
    keys: [AtomicU8; 768],
    key_tokens: [AtomicU64; 768],
    buttons: [AtomicU8; 5],
    button_tokens: [AtomicU64; 5],
    // Provenance for the existing primary token. Callback event order, not SendInput count,
    // determines which UP passes. No separate button ledger or callback locks are introduced.
    drag_order: AtomicU8,
    drag_first: AtomicU8,
    drag_nonce: AtomicUsize,
    drag_late_nonce: AtomicUsize,
    drag_token: AtomicU64,
    raw_x: AtomicI64,
    raw_y: AtomicI64,
    ring: Ring,
    sink: OnceLock<Arc<dyn EventSink<CaptureEvent>>>,
    events: Mutex<VecDeque<(Option<u64>, CaptureEvent)>>,
    owner_done: AtomicBool,
    delivery_done: AtomicBool,
    #[cfg(test)]
    fixture: AtomicUsize,
}

impl Shared {
    fn new(gate: Arc<IoGate>) -> Self {
        Self {
            gate,
            start: Instant::now(),
            generation: AtomicU64::new(4),
            active: AtomicU64::new(0),
            terminal: AtomicU64::new(0),
            terminal_token: AtomicU64::new(0),
            end_claim: AtomicBool::new(false),
            end_pending: AtomicBool::new(false),
            active_epoch: AtomicU64::new(0),
            warming: AtomicBool::new(false),
            reason: AtomicU8::new(0),
            shutdown: AtomicBool::new(false),
            fault: AtomicBool::new(false),
            fault_cause: AtomicU8::new(0),
            thread: AtomicU32::new(0),
            pump: AtomicU64::new(0),
            mouse_hook_count: AtomicU64::new(0),
            raw_count: AtomicU64::new(0),
            keyboard: AtomicUsize::new(0),
            mouse: AtomicUsize::new(0),
            window: AtomicUsize::new(0),
            cursor: AtomicUsize::new(0),
            clipped: AtomicBool::new(false),
            cleanup_verified: AtomicBool::new(true),
            nonce: AtomicUsize::new(0),
            ack: AtomicU8::new(0),
            probe_at: AtomicU64::new(0),
            probe_owed: AtomicBool::new(false),
            foreground_refused: AtomicBool::new(false),
            keys: std::array::from_fn(|_| AtomicU8::new(0)),
            key_tokens: std::array::from_fn(|_| AtomicU64::new(0)),
            buttons: std::array::from_fn(|_| AtomicU8::new(0)),
            button_tokens: std::array::from_fn(|_| AtomicU64::new(0)),
            drag_order: AtomicU8::new(0),
            drag_first: AtomicU8::new(0),
            drag_nonce: AtomicUsize::new(0),
            drag_late_nonce: AtomicUsize::new(0),
            drag_token: AtomicU64::new(0),
            raw_x: AtomicI64::new(0),
            raw_y: AtomicI64::new(0),
            ring: Ring::new(),
            sink: OnceLock::new(),
            events: Mutex::new(VecDeque::with_capacity(4097)),
            owner_done: AtomicBool::new(false),
            delivery_done: AtomicBool::new(false),
            #[cfg(test)]
            fixture: AtomicUsize::new(0),
        }
    }
    fn active_snapshot(&self) -> Option<(u64, u64)> {
        let control = self.generation.load(Ordering::Acquire);
        if control & 3 != model::ACTIVE
            || !self.gate.is_open()
            || self.gate.epoch() != self.active_epoch.load(Ordering::Acquire)
        {
            return None;
        }
        let id = self.active.load(Ordering::Acquire);
        (self.generation.load(Ordering::Acquire) == control).then_some((id, control))
    }
    fn active_id(&self) -> Option<u64> {
        self.active_snapshot().map(|s| s.0)
    }
    fn active_token(&self) -> Option<u64> {
        self.active_snapshot().map(|s| s.1)
    }
    /// Faults are terminal; the first site is kept for `lost`.
    fn fail(&self, cause: u8) {
        let _ = self
            .fault_cause
            .compare_exchange(0, cause, Ordering::AcqRel, Ordering::Acquire);
        self.fault.store(true, Ordering::Release);
    }
    /// `what` observation lost, with the first fault's site.
    fn lost(&self, what: &str) -> PlatformError {
        let cause = usize::from(self.fault_cause.load(Ordering::Acquire));
        PlatformError::Backend(format!(
            "{what} observation lost ({})",
            FAULT_CAUSES.get(cause).copied().unwrap_or("unrecorded")
        ))
    }
    fn enqueue(&self, id: Option<u64>, event: CaptureEvent) {
        let mut events = lock(&self.events);
        if events.len() >= 4096 && !matches!(event, CaptureEvent::Started { .. }) {
            self.fail(1);
            return;
        }
        events.push_back((id, event));
    }
    fn callback_record(&self, kind: u64, data: u64, capture: Option<(u64, u64)>) {
        if !self.ring.push(Record {
            kind,
            data,
            id: capture.map_or(0, |s| s.0),
            captured: capture.is_some(),
            token: capture.map_or(0, |s| s.1),
        }) {
            self.fail(2);
        }
        // SAFETY: posts a scalar wakeup to our live owner thread; no pointers or allocations.
        if unsafe { PostThreadMessageW(self.thread.load(Ordering::Acquire), WAKE, 0, 0) } == 0 {
            self.fail(3);
        }
    }
    /// Independent of owner/model locks. The delivery thread serializes the terminal event.
    fn terminate(&self, reason: EndReason) {
        if self.end_claim.swap(true, Ordering::AcqRel) {
            self.ungrab();
            return;
        }
        let previous = model::cancel(&self.generation);
        self.warming.store(false, Ordering::Release);
        if previous & 3 == model::ACTIVE {
            let id = self.active.load(Ordering::Acquire);
            self.reason.store(
                match reason {
                    EndReason::Requested => 1,
                    EndReason::Lost => 2,
                    EndReason::Aborted => 3,
                },
                Ordering::Release,
            );
            self.terminal.store(id, Ordering::Release);
            self.terminal_token.store(previous, Ordering::Release);
            self.end_pending.store(true, Ordering::Release);
        }
        self.ungrab();
        // SAFETY: asynchronous notification of our own window/thread; never waits for it.
        let thread = self.thread.load(Ordering::Acquire);
        if thread != 0 {
            // SAFETY: asynchronous wake of our recorded owner thread, never waits for it.
            unsafe {
                PostThreadMessageW(thread, WAKE, 0, 0);
            }
        }
        if previous & 3 != model::ACTIVE {
            self.end_claim.store(false, Ordering::Release);
        }
    }
    fn ungrab(&self) {
        if self.clipped.swap(false, Ordering::AcqRel) {
            // SAFETY: this backend set the clip and owns its release; no input is posted.
            if unsafe { ClipCursor(null()) } == 0 {
                // Preserve the release obligation for subsequent owned cleanup attempts.
                self.clipped.store(true, Ordering::Release);
                self.fail(4);
                self.cleanup_verified.store(false, Ordering::Release);
                eprintln!("crosspane capture pointer release unverified");
            }
        }
        let hwnd = self.window.load(Ordering::Acquire) as HWND;
        if !hwnd.is_null() {
            // SAFETY: hide only our owned cursor window without blocking on its owner thread.
            unsafe {
                ShowWindowAsync(hwnd, SW_HIDE);
            }
        }
        let cursor = self.cursor.load(Ordering::Acquire) as HCURSOR;
        // SAFETY: restore only if our owned transparent cursor is still current; this changes
        // the current cursor, never the user's system cursor scheme, and requires no owner loop.
        unsafe {
            if !cursor.is_null() && GetCursor() == cursor {
                SetCursor(LoadCursorW(null_mut(), IDC_ARROW));
            }
        }
    }
    fn admits(&self, token: Option<u64>, started: Option<u64>) -> bool {
        token.is_none() || (token == started && self.active_token() == token)
    }

    fn reserve_drag(
        &self,
        id: CaptureId,
        generation: u64,
        nonce: usize,
    ) -> Result<(), PlatformError> {
        if self.drag_order.load(Ordering::Acquire) != 0
            || self.buttons[1..]
                .iter()
                .any(|b| b.load(Ordering::Acquire) != 0)
            || self.buttons[0].load(Ordering::Acquire) != 1
            || self.generation.load(Ordering::Acquire) != generation
        {
            return Err(PlatformError::PointerButtonHeld);
        }
        self.active.store(id.0, Ordering::Release);
        self.drag_first.store(0, Ordering::Release);
        self.drag_nonce.store(nonce, Ordering::Release);
        self.drag_late_nonce.store(0, Ordering::Release);
        self.drag_token
            .store((generation & !3) | model::ACTIVE, Ordering::Release);
        self.button_tokens[0].store((generation & !3) | model::ACTIVE, Ordering::Release);
        self.buttons[0].store(2, Ordering::Release);
        self.drag_order.store(2, Ordering::Release);
        Ok(())
    }
    fn report_drag(&self, result: DragSettlementResult) {
        let order = self.drag_order.load(Ordering::Acquire);
        match result {
            DragSettlementResult::KnownZero if matches!(order, 2 | 4) => {
                if order == 2 {
                    self.buttons[0].store(1, Ordering::Release);
                }
                self.drag_order.store(0, Ordering::Release);
                self.drag_nonce.store(0, Ordering::Release);
            }
            DragSettlementResult::Uncertain if order == 2 => {
                self.drag_order.store(5, Ordering::Release)
            }
            _ => {}
        }
    }
    fn drag_up(&self, tagged: bool, nonce: usize) -> Option<bool> {
        let old = self.drag_order.load(Ordering::Acquire);
        if old == 0 || (tagged && (nonce == 0 || nonce != self.drag_nonce.load(Ordering::Acquire)))
        {
            return None;
        }
        let (next, swallow) = drag_up_transition(old, tagged);
        if tagged && old == next {
            return None;
        }
        self.drag_order.store(next, Ordering::Release);
        if matches!(old, 2 | 5) && matches!(next, 3 | 4) {
            self.drag_first.store(next, Ordering::Release);
        }
        if next == 0 {
            self.drag_nonce.store(0, Ordering::Release);
        }
        if !tagged {
            self.buttons[0].store(0, Ordering::Release);
        }
        Some(swallow)
    }
    fn drag_ready(&self, actual_end: bool) -> bool {
        actual_end
            && self.drag_order.load(Ordering::Acquire) == 3
            && self.buttons[0].load(Ordering::Acquire) == 2
            && self.buttons[1..]
                .iter()
                .all(|b| b.load(Ordering::Acquire) == 0)
    }
    fn expire_drag_tail(&self, nonce: usize) {
        crate::model::hook::expire_drag_shared(
            nonce,
            &self.drag_order,
            &self.drag_first,
            &self.drag_nonce,
            &self.drag_late_nonce,
            &self.drag_token,
            &self.buttons[0],
            &self.button_tokens[0],
        );
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        OWNER.store(false, Ordering::Release);
    }
}

struct Abort(Arc<Shared>);
impl CaptureAbort for Abort {
    fn abort(&self) {
        self.0.terminate(EndReason::Aborted);
    }
}

fn deliver(shared: Arc<Shared>) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| deliver_events(&shared)));
    if result.is_err() {
        shared.fail(5);
        note_loss(1);
        shared.terminate(EndReason::Lost);
        eprintln!("crosspane capture delivery failed");
    }
    shared.delivery_done.store(true, Ordering::Release);
}

fn deliver_events(shared: &Shared) {
    let mut started = None;
    loop {
        let item = lock(&shared.events).pop_front();
        if let Some((id, event)) = item {
            if let CaptureEvent::Started { .. } = event {
                started = id;
            } else if !shared.admits(id, started) {
                continue;
            }
            if let Some(sink) = shared.sink.get() {
                sink.send(event);
            }
        }
        let ended = shared.terminal.load(Ordering::Acquire);
        if shared.end_pending.load(Ordering::Acquire)
            && Some(shared.terminal_token.load(Ordering::Acquire)) == started
        {
            let reason = match shared.reason.load(Ordering::Acquire) {
                1 => EndReason::Requested,
                2 => EndReason::Lost,
                _ => EndReason::Aborted,
            };
            if let Some(sink) = shared.sink.get() {
                sink.send(CaptureEvent::Ended {
                    id: CaptureId(ended),
                    reason,
                });
            }
            started = None;
            shared.end_pending.store(false, Ordering::Release);
            shared.end_claim.store(false, Ordering::Release);
        }
        if shared.owner_done.load(Ordering::Acquire)
            && lock(&shared.events).is_empty()
            && !shared.end_pending.load(Ordering::Acquire)
        {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
}

unsafe extern "system" fn keyboard_callback(
    code: i32,
    message: WPARAM,
    parameter: LPARAM,
) -> LRESULT {
    let mut suppress = false;
    if code >= 0 {
        CALLBACK.with(|slot| {
            let shared = slot.get();
            if shared.is_null() {
                return;
            }
            // SAFETY: Windows supplies this hook record; TLS points to the owner's retained Arc.
            let (shared, key) = unsafe { (&*shared, &*(parameter as *const KBDLLHOOKSTRUCT)) };
            let injected = key.flags & (LLKHF_INJECTED | LLKHF_LOWER_IL_INJECTED) != 0;
            let nonce = shared.nonce.load(Ordering::Acquire);
            if injected
                && nonce != 0
                && key.dwExtraInfo == nonce
                && key.scanCode == model::F24_SCAN
                && key.vkCode == model::F24_VK
            {
                shared.ack.fetch_or(
                    if key.flags & LLKHF_UP != 0 { 2 } else { 1 },
                    Ordering::AcqRel,
                );
                suppress = true;
                return;
            }
            if injected {
                return;
            }
            let Some(index) =
                model::key_slot(key.scanCode, key.flags & LLKHF_EXTENDED != 0, key.vkCode)
            else {
                shared.fail(6);
                return;
            };
            let id = shared.active_snapshot();
            let up = key.flags & LLKHF_UP != 0;
            let state = shared.keys[index].load(Ordering::Relaxed);
            let (next, swallow) = model::key_transition(state, id.is_some(), up);
            let token = model::ledger_token(
                state,
                id.map(|s| s.1),
                shared.key_tokens[index].load(Ordering::Relaxed),
            );
            if next == 2 && state != 2 {
                shared.key_tokens[index].store(token.unwrap_or(0), Ordering::Relaxed);
            }
            shared.keys[index].store(next, Ordering::Release);
            suppress = swallow;
            shared.callback_record(
                1 | (u64::from(key.flags) << 8) | (u64::from(key.scanCode) << 40),
                u64::from(key.vkCode),
                id.zip(token).map(|(s, t)| (s.0, t)),
            );
        });
    }
    if suppress {
        1
    } else {
        // SAFETY: pass all non-owned injected and unsuppressed records to the hook chain.
        unsafe { CallNextHookEx(null_mut(), code, message, parameter) }
    }
}

unsafe extern "system" fn mouse_callback(code: i32, message: WPARAM, parameter: LPARAM) -> LRESULT {
    let mut suppress = false;
    if code >= 0 {
        CALLBACK.with(|slot| {
            let shared = slot.get();
            if shared.is_null() {
                return;
            }
            // SAFETY: system-provided record and retained owner-thread TLS Arc.
            let (shared, mouse) = unsafe { (&*shared, &*(parameter as *const MSLLHOOKSTRUCT)) };
            if mouse.flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED) != 0 {
                let nonce = shared.drag_nonce.load(Ordering::Acquire);
                if nonce != 0
                    && mouse.dwExtraInfo == nonce
                    && message as u32 == WM_LBUTTONUP
                    && let Some(swallow) = shared.drag_up(true, nonce)
                {
                    shared.callback_record(4, nonce as u64, None);
                    suppress = swallow;
                } else if message as u32 == WM_LBUTTONUP
                    && crate::model::hook::consume_late_drag_tag(
                        &shared.drag_late_nonce,
                        mouse.dwExtraInfo,
                    )
                {
                    shared.callback_record(4, mouse.dwExtraInfo as u64, None);
                    suppress = true;
                }
                return;
            }
            shared.mouse_hook_count.fetch_add(1, Ordering::AcqRel);
            let mut id = shared.active_snapshot();
            suppress = id.is_some();
            let button = match message as u32 {
                WM_LBUTTONDOWN => Some((0, false)),
                WM_LBUTTONUP => Some((0, true)),
                WM_RBUTTONDOWN => Some((1, false)),
                WM_RBUTTONUP => Some((1, true)),
                WM_MBUTTONDOWN => Some((2, false)),
                WM_MBUTTONUP => Some((2, true)),
                WM_XBUTTONDOWN | WM_XBUTTONUP => Some((
                    if mouse.mouseData >> 16 == 1 { 3 } else { 4 },
                    message as u32 == WM_XBUTTONUP,
                )),
                _ => None,
            };
            if let Some((index, up)) = button {
                let state = shared.buttons[index].load(Ordering::Relaxed);
                if index == 0
                    && up
                    && let Some(swallow) = shared.drag_up(false, 0)
                {
                    suppress = swallow;
                    let token = shared.button_tokens[0].load(Ordering::Acquire);
                    let point = u64::from(mouse.pt.x as u32) | (u64::from(mouse.pt.y as u32) << 32);
                    shared.callback_record(
                        2 | ((message as u64) << 8),
                        point,
                        if swallow {
                            id.map(|s| (s.0, token))
                        } else {
                            None
                        },
                    );
                    return;
                }
                let (next, swallow) = model::key_transition(state, id.is_some(), up);
                let token = model::ledger_token(
                    state,
                    id.map(|s| s.1),
                    shared.button_tokens[index].load(Ordering::Relaxed),
                );
                if next == 2 && state != 2 {
                    shared.button_tokens[index].store(token.unwrap_or(0), Ordering::Relaxed);
                }
                id = id.zip(token).map(|(s, t)| (s.0, t));
                shared.buttons[index].store(next, Ordering::Release);
                suppress = swallow;
            }
            let kind = 2 | ((message as u64) << 8) | (u64::from(mouse.mouseData) << 32);
            let point = u64::from(mouse.pt.x as u32) | (u64::from(mouse.pt.y as u32) << 32);
            shared.callback_record(kind, point, id);
        });
    }
    if suppress {
        1
    } else {
        // SAFETY: preserve the hook chain for all non-consumed input, including injected input.
        unsafe { CallNextHookEx(null_mut(), code, message, parameter) }
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if message == WM_INPUT {
        CALLBACK.with(|slot| {
            let pointer = slot.get();
            if pointer.is_null() {
                return;
            }
            // SAFETY: retained TLS ownership and a bounded stack buffer for a mouse RAWINPUT.
            unsafe {
                let shared = &*pointer;
                let mut input: RAWINPUT = std::mem::zeroed();
                let mut size = size_of::<RAWINPUT>() as u32;
                if GetRawInputData(
                    l as _,
                    RID_INPUT,
                    (&mut input as *mut RAWINPUT).cast(),
                    &mut size,
                    size_of::<RAWINPUTHEADER>() as u32,
                ) == u32::MAX
                {
                    shared.fail(7);
                    return;
                }
                if input.header.dwType != RIM_TYPEMOUSE || input.header.hDevice.is_null() {
                    return;
                }
                let mouse = input.data.mouse;
                let buttons = mouse.Anonymous.Anonymous;
                if mouse.lLastX != 0 || mouse.lLastY != 0 || buttons.usButtonFlags != 0 {
                    shared.raw_count.fetch_add(1, Ordering::AcqRel);
                }
                if mouse.usFlags & 1 != 0 {
                    shared.fail(8);
                    return;
                }
                shared
                    .raw_x
                    .fetch_add(i64::from(mouse.lLastX), Ordering::AcqRel);
                shared
                    .raw_y
                    .fetch_add(i64::from(mouse.lLastY), Ordering::AcqRel);
                if let Some(id) = shared.active_token() {
                    shared.enqueue(
                        Some(id),
                        CaptureEvent::Motion {
                            dx: f64::from(mouse.lLastX),
                            dy: f64::from(mouse.lLastY),
                            kind: MotionKind::Unaccelerated,
                            at: event_time(),
                        },
                    );
                }
            }
        });
    }
    // SAFETY: Windows default processing releases foreground WM_INPUT bookkeeping.
    unsafe { DefWindowProcW(hwnd, message, w, l) }
}

enum Command {
    Portals(
        Vec<(CapturePortal, PixelRect)>,
        Vec<(DisplayId, [i32; 4])>,
        mpsc::SyncSender<Result<(), PlatformError>>,
    ),
    Begin {
        id: CaptureId,
        portal: PortalId,
        generation: u64,
        epoch: u64,
        reply: mpsc::SyncSender<Result<CaptureStart, PlatformError>>,
    },
    BeginDrag {
        id: CaptureId,
        portal: PortalId,
        button: MouseButton,
        generation: u64,
        epoch: u64,
        reply: mpsc::SyncSender<Result<CaptureStart, PlatformError>>,
    },
    End(
        Option<(DisplayId, PointDevice)>,
        mpsc::SyncSender<Result<(), PlatformError>>,
    ),
    Subscribe(mpsc::SyncSender<Result<(), PlatformError>>),
    Monitor(bool, mpsc::SyncSender<Result<(), PlatformError>>),
}

/// Construction receives the same native monitor observations/ID allocator as the Windows
/// window-source/overlay adapters. Native monitor rectangles are rechecked before capture.
pub struct WindowsCapture {
    shared: Arc<Shared>,
    commands: mpsc::SyncSender<Command>,
    /// The table last accepted by `set_portals` (the startup table until then). `end()` warps with
    /// it and never refreshes: a refresh can block up to 2 s before input is given back.
    monitors: Vec<(DisplayId, [i32; 4])>,
    /// Re-reads the monitor table at each `set_portals`; `None` keeps the startup table.
    refresh: Option<crate::inject::MonitorRefresh>,
}

/// One native observation becomes the capture table: the non-twin displays in device-path order,
/// with their stable IDs. Construction and every `set_portals` refresh share these rules.
fn monitor_table(
    probes: &[MonitorProbe],
    ids: &mut DisplayIds,
) -> Result<Vec<(DisplayId, [i32; 4])>, PlatformError> {
    let layout = displays(probes, ids).map_err(|_| error("invalid capture display layout"))?;
    let mut sorted: Vec<_> = probes.iter().filter(|p| !p.twin).collect();
    sorted.sort_by(|a, b| a.device_path.cmp(&b.device_path));
    let monitors: Vec<_> = layout
        .displays
        .iter()
        .zip(sorted)
        .map(|(d, p)| (d.id, p.rc_monitor))
        .collect();
    if monitors.iter().any(|(_, r)| {
        r[2].checked_sub(r[0]).is_none_or(|w| w <= 0)
            || r[3].checked_sub(r[1]).is_none_or(|h| h <= 0)
    }) {
        return Err(error("unrepresentable capture monitor"));
    }
    if monitors.is_empty() {
        return Err(error("no capture display"));
    }
    Ok(monitors)
}

/// Only a non-empty set reads the displays. A CLEAR (no portals) keeps the last accepted table, so
/// a dead or slow display read cannot stop old strips from being cleared.
fn portals_need_refresh(portals: &[CapturePortal]) -> bool {
    !portals.is_empty()
}

/// Maps each portal to its one-pixel strip on `monitors`. A portal on an unknown display, or a
/// stretch outside that display, refuses the whole set.
fn portal_strips(
    portals: &[CapturePortal],
    monitors: &[(DisplayId, [i32; 4])],
) -> Result<Vec<(CapturePortal, PixelRect)>, PlatformError> {
    let mut physical = Vec::new();
    for &portal in portals {
        let rect = monitors
            .iter()
            .find(|m| m.0 == portal.display)
            .ok_or(PlatformError::NotFound)?
            .1;
        let length = if matches!(portal.edge, Edge::Left | Edge::Right) {
            rect[3] - rect[1]
        } else {
            rect[2] - rect[0]
        };
        if !portal.from.is_finite()
            || !portal.to.is_finite()
            || portal.from < 0.0
            || portal.to > f64::from(length)
            || portal.from >= portal.to
        {
            return Err(error("invalid capture portal"));
        }
        let from = portal.from.ceil() as i32;
        let to = portal.to.ceil() as i32;
        let (x, y, w, h) = match portal.edge {
            Edge::Left => (rect[0], rect[1] + from, 1, to - from),
            Edge::Right => (rect[2] - 1, rect[1] + from, 1, to - from),
            Edge::Top => (rect[0] + from, rect[1], to - from, 1),
            Edge::Bottom => (rect[0] + from, rect[3] - 1, to - from, 1),
        };
        physical.push((
            portal,
            PixelRect::new(
                crosspane_types::geom::euclid::point2(x, y),
                crosspane_types::geom::euclid::point2(x + w, y + h),
            ),
        ));
    }
    Ok(physical)
}

impl std::fmt::Debug for WindowsCapture {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WindowsCapture")
            .finish_non_exhaustive()
    }
}

impl WindowsCapture {
    pub fn new(
        gate: Arc<IoGate>,
        probes: &[MonitorProbe],
        ids: &mut DisplayIds,
    ) -> Result<Self, PlatformError> {
        Self::construct(gate, probes, ids, None)
    }

    /// Additive drag detection using the SAME retained source and SAME injection gate/runtime.
    pub fn new_with_drag(
        gate: Arc<IoGate>,
        probes: &[MonitorProbe],
        ids: &mut DisplayIds,
        drag: crate::drag::DragConfig,
    ) -> Result<Self, PlatformError> {
        if !drag.same_gate(&gate) {
            return Err(error("drag source gate unavailable or mismatched"));
        }
        Self::construct(gate, probes, ids, Some(drag))
    }

    fn construct(
        gate: Arc<IoGate>,
        probes: &[MonitorProbe],
        ids: &mut DisplayIds,
        drag: Option<crate::drag::DragConfig>,
    ) -> Result<Self, PlatformError> {
        if OWNER
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(error("capture owner already exists"));
        }
        let monitors = match monitor_table(probes, ids) {
            Ok(monitors) => monitors,
            Err(e) => {
                OWNER.store(false, Ordering::Release);
                return Err(e);
            }
        };
        // The owner slot is ours, so this backend starts with no recorded loss: a rebuilt backend
        // must never report the cause of the one before it.
        LAST_LOSS.store(0, Ordering::Release);
        let shared = Arc::new(Shared::new(gate));
        let (commands, receive) = mpsc::sync_channel(16);
        let (ready, wait) = mpsc::sync_channel(1);
        let owner = shared.clone();
        let geometry = monitors.clone();
        if thread::Builder::new()
            .name("crosspane-capture".into())
            .spawn(move || owner_thread(owner, geometry, receive, ready, drag))
            .is_err()
        {
            OWNER.store(false, Ordering::Release);
            return Err(error("capture thread unavailable"));
        }
        match wait.recv_timeout(COMMAND_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                shared.shutdown.store(true, Ordering::Release);
                return Err(e);
            }
            Err(_) => {
                shared.shutdown.store(true, Ordering::Release);
                shared.terminate(EndReason::Aborted);
                return Err(PlatformError::Timeout);
            }
        }
        let delivery = shared.clone();
        if thread::Builder::new()
            .name("crosspane-capture-events".into())
            .spawn(move || deliver(delivery))
            .is_err()
        {
            shared.shutdown.store(true, Ordering::Release);
            shared.terminate(EndReason::Lost);
            return Err(error("capture delivery unavailable"));
        }
        Ok(Self {
            shared,
            commands,
            monitors,
            refresh: None,
        })
    }

    /// Each `set_portals` re-reads the monitor table from `refresh`; without it the startup table
    /// is used (tests).
    pub fn with_monitor_refresh(mut self, refresh: crate::inject::MonitorRefresh) -> Self {
        self.refresh = Some(refresh);
        self
    }

    /// Whether this backend can never serve another request: its owner thread has ended, or a
    /// fault is recorded (faults are terminal; every later request answers "observation lost").
    /// Two atomic loads, safe from any thread.
    pub fn is_dead(&self) -> bool {
        is_dead(&self.shared)
    }

    /// A handle for asking `is_dead` after this backend has been boxed as a `dyn InputCapture`.
    /// It holds no strong reference, so it never keeps a dropped backend (or the process-wide
    /// owner slot) alive.
    pub fn health(&self) -> CaptureHealth {
        CaptureHealth(Arc::downgrade(&self.shared))
    }

    fn request<T>(
        &self,
        command: impl FnOnce(mpsc::SyncSender<Result<T, PlatformError>>) -> Command,
    ) -> Result<T, PlatformError> {
        if self.shared.owner_done.load(Ordering::Acquire)
            || self.shared.fault.load(Ordering::Acquire)
        {
            return Err(self.shared.lost("capture"));
        }
        let (reply, wait) = mpsc::sync_channel(1);
        self.commands
            .try_send(command(reply))
            .map_err(|_| error("capture command unavailable"))?;
        // SAFETY: wake only the owned thread; the queue already owns the command payload.
        unsafe {
            PostThreadMessageW(self.shared.thread.load(Ordering::Acquire), WAKE, 0, 0);
        }
        match wait.recv_timeout(COMMAND_TIMEOUT) {
            Ok(result) => result,
            Err(_) => {
                self.shared.terminate(EndReason::Aborted);
                Err(PlatformError::Timeout)
            }
        }
    }
}

impl InputCapture for WindowsCapture {
    fn set_portals(&mut self, portals: &[CapturePortal]) -> Result<(), PlatformError> {
        // Any refresh, table or portal failure refuses the whole set. The previous table stays in
        // both places; Begin's live rectangle check refuses if a display no longer matches it.
        // A CLEAR (empty set) skips the refresh and reuses the last accepted table.
        let fresh = match &self.refresh {
            Some(refresh) if portals_need_refresh(portals) => {
                let (probes, mut ids) = refresh()?;
                monitor_table(&probes, &mut ids)?
            }
            _ => self.monitors.clone(),
        };
        let strips = portal_strips(portals, &fresh)?;
        self.request(|reply| Command::Portals(strips, fresh.clone(), reply))?;
        self.monitors = fresh;
        Ok(())
    }
    fn subscribe(&mut self, sink: Arc<dyn EventSink<CaptureEvent>>) -> Result<(), PlatformError> {
        self.shared
            .sink
            .set(sink)
            .map_err(|_| error("capture already subscribed"))?;
        self.request(Command::Subscribe)
    }
    fn begin(&mut self, id: CaptureId, portal: PortalId) -> Result<CaptureStart, PlatformError> {
        if !self.shared.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        let current = self.shared.generation.load(Ordering::Acquire);
        if current & 3 != model::IDLE
            || self.shared.sink.get().is_none()
            || self.shared.end_claim.load(Ordering::Acquire)
            || self.shared.probe_owed.load(Ordering::Acquire)
        {
            return Err(error("capture not ready"));
        }
        let generation = current | model::PENDING;
        self.shared
            .generation
            .compare_exchange(current, generation, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| error("capture activation cancelled"))?;
        let epoch = self.shared.gate.epoch();
        let result = self.request(|reply| Command::Begin {
            id,
            portal,
            generation,
            epoch,
            reply,
        });
        if result.is_err() {
            self.shared.terminate(EndReason::Aborted);
        }
        result
    }
    fn begin_drag(
        &mut self,
        id: CaptureId,
        portal: PortalId,
        button: MouseButton,
    ) -> Result<CaptureStart, PlatformError> {
        if !self.shared.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        let current = self.shared.generation.load(Ordering::Acquire);
        if current & 3 != model::IDLE
            || self.shared.sink.get().is_none()
            || self.shared.end_claim.load(Ordering::Acquire)
            || self.shared.probe_owed.load(Ordering::Acquire)
            || self.shared.drag_order.load(Ordering::Acquire) != 0
        {
            return Err(error("drag capture not ready"));
        }
        let generation = current | model::PENDING;
        self.shared
            .generation
            .compare_exchange(current, generation, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| error("drag activation cancelled"))?;
        let epoch = self.shared.gate.epoch();
        let result = self.request(|reply| Command::BeginDrag {
            id,
            portal,
            button,
            generation,
            epoch,
            reply,
        });
        if result.is_err() {
            self.shared.terminate(EndReason::Aborted);
        }
        result
    }
    fn end(&mut self, warp: Option<(DisplayId, PointDevice)>) -> Result<(), PlatformError> {
        // Restore before publishing Ended; every warp failure still ends suppression.
        let restored = (|| {
            if let Some((display, point)) = warp {
                let rect = self
                    .monitors
                    .iter()
                    .find(|m| m.0 == display)
                    .ok_or(PlatformError::NotFound)?
                    .1;
                if !point.x.is_finite()
                    || !point.y.is_finite()
                    || point.x < 0.0
                    || point.y < 0.0
                    || point.x >= f64::from(rect[2] - rect[0])
                    || point.y >= f64::from(rect[3] - rect[1])
                {
                    return Err(error("invalid capture warp"));
                }
                self.shared.ungrab();
                // SAFETY: caller-requested restoration, no foreign window/keyboard operation.
                if unsafe {
                    SetCursorPos(
                        rect[0] + (point.x.round() as i32).min(rect[2] - rect[0] - 1),
                        rect[1] + (point.y.round() as i32).min(rect[3] - rect[1] - 1),
                    )
                } == 0
                {
                    return Err(error("capture warp failed"));
                }
            }
            Ok(())
        })();
        self.shared.terminate(EndReason::Requested);
        let ended = self.request(|reply| Command::End(None, reply));
        restored.and(ended)
    }
    fn abort_handle(&self) -> Arc<dyn CaptureAbort> {
        Arc::new(Abort(self.shared.clone()))
    }
    fn set_monitor_local_activity(&mut self, on: bool) -> Result<(), PlatformError> {
        self.request(|reply| Command::Monitor(on, reply))
    }
}

impl Drop for WindowsCapture {
    fn drop(&mut self) {
        self.shared.terminate(EndReason::Aborted);
        self.shared.shutdown.store(true, Ordering::Release);
        // No join on the native thread: its retained Arc owns all callback resources until exit.
        // A dead backend's owner has already torn the hooks, the raw registration and the clip
        // down (before `owner_done`), and its helper threads leave on `owner_done`; the owner slot
        // is released when the last of them drops its `Arc<Shared>`. `wait_owner_released` is how
        // a caller that wants to build a replacement waits for exactly that.
    }
}

fn is_dead(shared: &Shared) -> bool {
    shared.owner_done.load(Ordering::Acquire) || shared.fault.load(Ordering::Acquire)
}

/// Asks whether a backend can never serve another request (`WindowsCapture::is_dead`) without
/// owning it. Weak: it neither keeps the backend's state alive nor delays the release of the
/// process-wide owner slot.
#[derive(Clone, Debug)]
pub struct CaptureHealth(std::sync::Weak<Shared>);

impl CaptureHealth {
    /// `Some(cause)` once the backend can never serve another request. The cause is a fixed label
    /// (the first fault's site, or that the owner thread ended): never key, button or pointer
    /// data. `None` while the backend is serving.
    pub fn lost(&self) -> Option<&'static str> {
        let Some(shared) = self.0.upgrade() else {
            return Some("backend released");
        };
        if shared.fault.load(Ordering::Acquire) {
            let cause = usize::from(shared.fault_cause.load(Ordering::Acquire));
            return Some(FAULT_CAUSES.get(cause).copied().unwrap_or("unrecorded"));
        }
        shared
            .owner_done
            .load(Ordering::Acquire)
            .then_some("owner thread ended")
    }
}

/// Waits until no capture owner exists in this process, so a new `WindowsCapture` can be built.
/// The slot is released when the last thread of a dropped backend lets go of its state; for a dead
/// backend that is the owner (after it removed its hooks, raw registration and clip), the
/// watchdog, the probe and the delivery thread, each of which ends on `owner_done` within tens of
/// milliseconds. `false` when the slot is still held after `timeout` (for example an owner still
/// containing the tail of a physical drag): the caller may try again later.
pub fn wait_owner_released(timeout: Duration) -> bool {
    let deadline = Instant::now().checked_add(timeout);
    while OWNER.load(Ordering::Acquire) {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return false;
        }
        thread::sleep(Duration::from_millis(2));
    }
    true
}

fn mouse_target() -> Result<Option<HWND>, PlatformError> {
    // SAFETY: bounded read-only native registration inventory, no keyboard registration.
    unsafe {
        let mut count = 0;
        if GetRegisteredRawInputDevices(null_mut(), &mut count, size_of::<RAWINPUTDEVICE>() as u32)
            == u32::MAX
            || count > 64
        {
            return Err(error("raw registration unavailable"));
        }
        let mut devices = vec![std::mem::zeroed::<RAWINPUTDEVICE>(); count as usize];
        if count != 0
            && GetRegisteredRawInputDevices(
                devices.as_mut_ptr(),
                &mut count,
                size_of::<RAWINPUTDEVICE>() as u32,
            ) == u32::MAX
        {
            return Err(error("raw registration unavailable"));
        }
        let targets: Vec<_> = devices
            .into_iter()
            .filter(|d| d.usUsagePage == 1 && (d.usUsage == 2 || d.usUsage == 0))
            .map(|d| d.hwndTarget)
            .collect();
        if targets.len() > 1 {
            return Err(error("ambiguous mouse registration"));
        }
        Ok(targets.first().copied())
    }
}

fn set_raw(hwnd: HWND, add: bool) -> Result<(), PlatformError> {
    if (add && mouse_target()?.is_some()) || (!add && mouse_target()? != Some(hwnd)) {
        return Err(error("mouse registration ownership lost"));
    }
    let device = RAWINPUTDEVICE {
        usUsagePage: 1,
        usUsage: 2,
        dwFlags: if add { RIDEV_INPUTSINK } else { RIDEV_REMOVE },
        hwndTarget: if add { hwnd } else { null_mut() },
    };
    // SAFETY: solely own mouse class; REMOVE uses the required NULL target. Keyboard is untouched.
    if unsafe { RegisterRawInputDevices(&device, 1, size_of::<RAWINPUTDEVICE>() as u32) } == 0 {
        Err(error("mouse raw registration failed"))
    } else {
        Ok(())
    }
}

struct Native {
    shared: Arc<Shared>,
    hwnd: HWND,
    cursor: HCURSOR,
    class: Vec<u16>,
    instance: windows_sys::Win32::Foundation::HINSTANCE,
}

impl Native {
    fn new(shared: Arc<Shared>) -> Result<Self, PlatformError> {
        // SAFETY: current module handle, owned class and owned transparent cursor/window.
        unsafe {
            let instance = GetModuleHandleW(null());
            if instance.is_null() {
                return Err(error("capture module unavailable"));
            }
            let class: Vec<u16> = format!("CrosspaneCapture-{}", GetCurrentThreadId())
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let and = [255_u8; 128];
            let xor = [0_u8; 128];
            let cursor = CreateCursor(
                instance,
                0,
                0,
                32,
                32,
                and.as_ptr().cast(),
                xor.as_ptr().cast(),
            );
            if cursor.is_null() {
                return Err(error("transparent cursor unavailable"));
            }
            let wc = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                hCursor: cursor,
                ..std::mem::zeroed()
            };
            if RegisterClassW(&wc) == 0 {
                DestroyCursor(cursor);
                return Err(error("capture class unavailable"));
            }
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST | WS_EX_LAYERED,
                class.as_ptr(),
                class.as_ptr(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            if hwnd.is_null() {
                UnregisterClassW(class.as_ptr(), instance);
                DestroyCursor(cursor);
                return Err(error("capture window unavailable"));
            }
            // Alpha 1 preserves cursor hit testing: fully zero-alpha layered pixels pass through.
            if SetLayeredWindowAttributes(hwnd, 0, 1, LWA_ALPHA) == 0 {
                DestroyWindow(hwnd);
                UnregisterClassW(class.as_ptr(), instance);
                DestroyCursor(cursor);
                return Err(error("cursor window transparency failed"));
            }
            shared.window.store(hwnd as usize, Ordering::Release);
            shared.cursor.store(cursor as usize, Ordering::Release);
            Ok(Self {
                shared,
                hwnd,
                cursor,
                class,
                instance,
            })
        }
    }
    fn step(&self, resource: Resource, add: bool, point: POINT) -> Result<(), PlatformError> {
        // SAFETY: all handles belong to this native owner; hooks point at retained TLS state.
        unsafe {
            match resource {
                Resource::KeyboardHook | Resource::MouseHook => {
                    let slot = if resource == Resource::KeyboardHook {
                        &self.shared.keyboard
                    } else {
                        &self.shared.mouse
                    };
                    if add {
                        if slot.load(Ordering::Acquire) != 0 {
                            return Ok(());
                        }
                        let handle = SetWindowsHookExW(
                            if resource == Resource::KeyboardHook {
                                WH_KEYBOARD_LL
                            } else {
                                WH_MOUSE_LL
                            },
                            Some(if resource == Resource::KeyboardHook {
                                keyboard_callback
                            } else {
                                mouse_callback
                            }),
                            self.instance,
                            0,
                        );
                        if handle.is_null() {
                            return Err(error("capture hook installation failed"));
                        }
                        slot.store(handle as usize, Ordering::Release);
                    } else {
                        let handle = slot.swap(0, Ordering::AcqRel) as HHOOK;
                        if !handle.is_null() && UnhookWindowsHookEx(handle) == 0 {
                            return Err(error("capture hook removal unverified"));
                        }
                    }
                }
                Resource::MouseRaw => {
                    if add && mouse_target()? == Some(self.hwnd) {
                        return Ok(());
                    }
                    if add || mouse_target()? == Some(self.hwnd) {
                        set_raw(self.hwnd, add)?;
                    }
                }
                Resource::CursorWindow => {
                    if add {
                        if SetWindowPos(
                            self.hwnd,
                            HWND_TOPMOST,
                            point.x,
                            point.y,
                            1,
                            1,
                            SWP_NOACTIVATE | SWP_SHOWWINDOW,
                        ) == 0
                        {
                            return Err(error("cursor window placement failed"));
                        }
                        SetCursor(self.cursor);
                    } else {
                        ShowWindow(self.hwnd, SW_HIDE);
                        if GetCursor() == self.cursor {
                            SetCursor(LoadCursorW(null_mut(), IDC_ARROW));
                        }
                    }
                }
                Resource::PointerClip => {
                    if add {
                        let rect = RECT {
                            left: point.x,
                            top: point.y,
                            right: point
                                .x
                                .checked_add(1)
                                .ok_or_else(|| error("invalid cursor point"))?,
                            bottom: point
                                .y
                                .checked_add(1)
                                .ok_or_else(|| error("invalid cursor point"))?,
                        };
                        self.shared.clipped.store(true, Ordering::Release);
                        if ClipCursor(&rect) == 0 {
                            return Err(error("pointer clip failed"));
                        }
                    } else {
                        self.shared.ungrab();
                    }
                }
            }
            Ok(())
        }
    }
    fn cleanup(&self) {
        for resource in [
            Resource::PointerClip,
            Resource::CursorWindow,
            Resource::MouseRaw,
            Resource::MouseHook,
            Resource::KeyboardHook,
        ] {
            if self.step(resource, false, POINT { x: 0, y: 0 }).is_err() {
                self.shared.cleanup_verified.store(false, Ordering::Release);
                eprintln!("crosspane capture cleanup unverified");
            }
        }
    }
    fn release_capture(&self) {
        for resource in [Resource::PointerClip, Resource::CursorWindow] {
            if self.step(resource, false, POINT { x: 0, y: 0 }).is_err() {
                self.shared.cleanup_verified.store(false, Ordering::Release);
                eprintln!("crosspane capture ungrab unverified");
            }
        }
    }
    fn capture_healthy(&self, point: POINT) -> bool {
        // SAFETY: read-only clip/cursor proof; compare solely our rectangle and owned cursor.
        unsafe {
            let mut clip: RECT = std::mem::zeroed();
            let mut cursor: CURSORINFO = std::mem::zeroed();
            cursor.cbSize = size_of::<CURSORINFO>() as u32;
            GetClipCursor(&mut clip) != 0
                && [clip.left, clip.top, clip.right, clip.bottom]
                    == [point.x, point.y, point.x + 1, point.y + 1]
                && GetCursorInfo(&mut cursor) != 0
                && cursor.hCursor == self.cursor
        }
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        self.cleanup();
        self.shared.window.store(0, Ordering::Release);
        self.shared.cursor.store(0, Ordering::Release);
        // SAFETY: owner-thread destruction of only this class/window/cursor after hook teardown.
        unsafe {
            DestroyWindow(self.hwnd);
            UnregisterClassW(self.class.as_ptr(), self.instance);
            DestroyCursor(self.cursor);
        }
    }
}

/// `[P]` Lock bits come only from this owned pumping thread: a GetKeyState read after its own pump,
/// and only once `fresh` (a resync posted by one pump is handled by the next). Resync through the
/// pumping thread (posted key-state update events) is not documented by Microsoft. Lead probe
/// 2026-10-11: a pumping-thread GetKeyState read tracked every toggle within one 250 ms sample. An
/// unfresh read is unknown (all `None`). Values are never logged.
/// <https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getkeystate>
fn lock_keys(fresh: bool) -> LockKeys {
    if !fresh {
        return LockKeys::default();
    }
    // SAFETY: read-only GetKeyState on this owned pumping thread, after its pump. Only the toggle
    // bits are used; the values are never logged.
    let states =
        unsafe { [VK_CAPITAL, VK_NUMLOCK, VK_SCROLL].map(|vk| GetKeyState(i32::from(vk))) };
    locks_from_states(states)
}

/// Pure toggle-bit decode: bit 0 is the toggle state; the high "down" bit is ignored.
fn locks_from_states(states: [i16; 3]) -> LockKeys {
    let [caps, num, scroll] = states.map(|state| Some(state & 1 != 0));
    LockKeys {
        caps_lock: caps,
        num_lock: num,
        scroll_lock: scroll,
    }
}

fn snapshot(shared: &Shared) -> (Vec<KeySnapshot>, [bool; 256]) {
    let mut held = Vec::new();
    let mut buttons = [false; 256];
    // SAFETY: observation outside LL callbacks; VK->scan preserves extended prefix, no text API.
    unsafe {
        for vk in 8..=255 {
            if GetAsyncKeyState(vk) >= 0 {
                continue;
            }
            let scan = MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC_EX);
            let identity = ((scan & 255) as u8, scan >> 8 == 0xe0, vk as u32);
            if let Some(slot) = model::key_slot(u32::from(identity.0), identity.1, identity.2) {
                if shared.keys[slot].load(Ordering::Acquire) != 2 {
                    shared.keys[slot].store(1, Ordering::Release);
                }
                held.push(identity);
            }
        }
        for (index, vk) in [1, 2, 4, 5, 6].into_iter().enumerate() {
            buttons[index] = GetAsyncKeyState(vk) < 0;
        }
    }
    (held, buttons)
}

fn nonce() -> Result<usize, PlatformError> {
    let mut value = 0_usize;
    // SAFETY: system CSPRNG writes exactly one initialized usize; no keys or persistent state.
    if unsafe {
        BCryptGenRandom(
            null_mut(),
            (&mut value as *mut usize).cast(),
            size_of::<usize>() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    } < 0
        || value == 0
    {
        Err(error("capture probe identity unavailable"))
    } else {
        Ok(value)
    }
}

/// Classifies the current foreground window against this process's integrity (see
/// `ForegroundClass`). Mirrors the injector's foreground read (`inject.rs` `foreground`): window,
/// pid and thread, the pid's token integrity, then the window and pid again. A foreground that
/// changes while it is read is read again (three attempts) and otherwise reads as unknown. Our own
/// process is at our own integrity without a query. A null foreground, which is what the secure
/// desktop and another input desktop look like to this process, is `Absent`. Reads identity only:
/// no titles, content or input.
fn foreground_class() -> ForegroundClass {
    let own = own_integrity();
    for _ in 0..3 {
        // SAFETY: read-only foreground identity queries; no titles or content are acquired.
        let (window, pid) = unsafe {
            let window = GetForegroundWindow();
            let mut pid = 0;
            if !window.is_null() && GetWindowThreadProcessId(window, &mut pid) == 0 {
                pid = 0;
            }
            (window, pid)
        };
        if window.is_null() {
            return ForegroundClass::Absent;
        }
        if pid == 0 {
            continue;
        }
        // SAFETY: reads only this process's id.
        let foreground = if pid == unsafe { GetCurrentProcessId() } {
            own
        } else {
            process_integrity(pid)
        };
        // SAFETY: read-only recheck that the same window still belongs to the same process.
        let unchanged = unsafe {
            let mut again = 0;
            GetForegroundWindow() == window
                && GetWindowThreadProcessId(window, &mut again) != 0
                && again == pid
        };
        if unchanged {
            return model::classify_foreground(true, foreground, own);
        }
    }
    ForegroundClass::Unknown
}

fn probe_phase(shared: &Shared) -> ProbePhase {
    if shared.warming.load(Ordering::Acquire) {
        ProbePhase::Warming
    } else {
        ProbePhase::Active
    }
}

/// Ends the capture as `Lost` because its foreground is elevated or secure and input cannot be
/// verified. Deliberately not a fault: nothing is wrong with the backend, so it stays usable and
/// the user simply gets control back on this machine. Does nothing when no capture is live (a
/// stale decision for a capture that already ended must not mislabel it or cancel a new one).
fn end_for_elevated_foreground(shared: &Shared) {
    let live = shared.warming.load(Ordering::Acquire)
        || shared.generation.load(Ordering::Acquire) & 3 == model::ACTIVE;
    if live {
        note_loss(LOSS_ELEVATED);
        shared.terminate(EndReason::Lost);
    }
}

fn submit_probe(_shared: &Shared, nonce: usize, up_only: bool) -> u32 {
    #[cfg(test)]
    {
        let fixture = _shared.fixture.load(Ordering::Acquire) as HWND;
        if !fixture.is_null() {
            let mut pid = 0;
            // SAFETY: compare only our authenticated fixture HWND/PID, no foreign metadata.
            let owned = unsafe {
                GetWindowThreadProcessId(fixture, &mut pid);
                GetForegroundWindow() == fixture
                    && pid == windows_sys::Win32::System::Threading::GetCurrentProcessId()
            };
            if !owned {
                return 0;
            }
        }
    }
    if !up_only {
        // SAFETY: never disturb a known already-held F24. Zero conflates up/failure [U]; gate
        // admission and positive owned acknowledgment are separate, mandatory checks.
        if unsafe { GetAsyncKeyState(model::F24_VK as i32) } < 0 {
            return 0;
        }
    }
    let key = |up| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0,
                wScan: model::F24_SCAN as u16,
                dwFlags: KEYEVENTF_SCANCODE | if up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: nonce,
            },
        },
    };
    let pair = [key(false), key(true)];
    // SAFETY: owned nonce pair/owed-up only; ledger is recorded BEFORE entry. No arbitrary input.
    unsafe {
        SendInput(
            if up_only { 1 } else { 2 },
            if up_only {
                pair[1..].as_ptr()
            } else {
                pair.as_ptr()
            },
            size_of::<INPUT>() as i32,
        )
    }
}

fn prepare_capture(
    shared: &Shared,
    native: &Native,
    probe_tx: &mpsc::SyncSender<Option<(usize, u64)>>,
    generation: u64,
    epoch: u64,
    point: POINT,
    nonce: usize,
) -> Result<(), PlatformError> {
    // UIPI would silently drop the warm-up probe aimed at an elevated or secure foreground, so
    // the capture could never be verified: refuse now, before any hook, clip or cursor is taken.
    // A refusal, not a fault; the caller rolls back and the backend stays usable.
    if model::probe_decision(
        foreground_class(),
        ProbePhase::Warming,
        ProbeStage::BeforeSubmit,
    ) == ProbeDecision::Refuse
    {
        return Err(PlatformError::SecureInput);
    }
    shared.foreground_refused.store(false, Ordering::Release);
    shared.probe_at.store(0, Ordering::Release);
    shared.ack.store(0, Ordering::Release);
    shared.active_epoch.store(epoch, Ordering::Release);
    shared.warming.store(true, Ordering::Release);
    model::acquire(|r, add| {
        if add
            && (generation != shared.generation.load(Ordering::Acquire)
                || !shared.gate.is_open()
                || shared.gate.epoch() != epoch)
        {
            return Err(PlatformError::Locked);
        }
        // Lifetime observation hooks stay installed to contain the matching physical tail.
        if add || matches!(r, Resource::CursorWindow | Resource::PointerClip) {
            native.step(r, add, point)?;
        }
        if add
            && (generation != shared.generation.load(Ordering::Acquire)
                || !shared.gate.is_open()
                || shared.gate.epoch() != epoch)
        {
            return Err(PlatformError::Locked);
        }
        Ok(())
    })?;
    shared.nonce.store(nonce, Ordering::Release);
    shared.ack.store(0, Ordering::Release);
    probe_tx
        .try_send(Some((nonce, epoch)))
        .map_err(|_| error("probe unavailable"))
}

struct Pending {
    id: CaptureId,
    portal: PortalId,
    generation: u64,
    epoch: u64,
    started: u64,
    point: POINT,
    drag: Option<crate::window::NativeWindow>,
    nonce: usize,
    prepared: bool,
    reply: mpsc::SyncSender<Result<CaptureStart, PlatformError>>,
}

fn owner_thread(
    shared: Arc<Shared>,
    monitors: Vec<(DisplayId, [i32; 4])>,
    commands: mpsc::Receiver<Command>,
    ready: mpsc::SyncSender<Result<(), PlatformError>>,
    drag: Option<crate::drag::DragConfig>,
) {
    // Unwinding retains native ownership until this function's guard releases it.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_owner(&shared, monitors, commands, &ready, drag)
    }));
    if result.is_err() {
        shared.fail(9);
        note_loss(1);
        shared.terminate(EndReason::Lost);
        eprintln!("crosspane capture owner failed");
    }
    shared.owner_done.store(true, Ordering::Release);
}

fn run_owner(
    shared: &Arc<Shared>,
    mut monitors: Vec<(DisplayId, [i32; 4])>,
    commands: mpsc::Receiver<Command>,
    ready: &mpsc::SyncSender<Result<(), PlatformError>>,
    drag: Option<crate::drag::DragConfig>,
) {
    // SAFETY: changes only our owner thread; all cursor/monitor coordinates are physical.
    if unsafe {
        windows_sys::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(
            windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        )
    }
    .is_null()
    {
        let _ = ready.send(Err(error("capture DPI context unavailable")));
        return;
    }
    // SAFETY: initialize our thread message queue and set only our own thread priority.
    unsafe {
        let mut message: MSG = std::mem::zeroed();
        PeekMessageW(&mut message, null_mut(), 0, 0, PM_NOREMOVE);
        shared.thread.store(GetCurrentThreadId(), Ordering::Release);
        if SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) == 0 {
            let _ = ready.send(Err(error("capture priority unavailable")));
            return;
        }
    }
    CALLBACK.with(|slot| slot.set(Arc::as_ptr(shared)));
    let native = match Native::new(shared.clone()) {
        Ok(native) => native,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    if mouse_target().map_or(true, |target| target.is_some()) {
        let _ = ready.send(Err(error("mouse raw registration already owned")));
        return;
    }
    for resource in [
        Resource::KeyboardHook,
        Resource::MouseHook,
        Resource::MouseRaw,
    ] {
        if let Err(e) = native.step(resource, true, POINT { x: 0, y: 0 }) {
            let _ = ready.send(Err(e));
            return;
        }
    }
    let mut drag = match drag.map(crate::drag::Observer::new).transpose() {
        Ok(drag) => drag,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let mut hook = HookState::new();
    let mut pending: Option<Pending> = None;
    let mut monitor = false;
    let mut locks = lock_keys(false);
    let mut old_active = None;
    let mut portals = Vec::new();
    let mut clip_point: Option<POINT> = None;
    let (probe_tx, probe_rx) = mpsc::sync_channel::<Option<(usize, u64)>>(4);
    let health = shared.clone();
    if thread::Builder::new()
        .name("crosspane-capture-probe".into())
        .spawn(move || probe_thread(health, probe_rx))
        .is_err()
    {
        shared.fail(10);
        return;
    }
    let health = shared.clone();
    if thread::Builder::new()
        .name("crosspane-capture-watchdog".into())
        .spawn(move || {
            let mut watch: Option<Watchdog> = None;
            while !health.owner_done.load(Ordering::Acquire) {
                let active = health.generation.load(Ordering::Acquire) & 3 == model::ACTIVE;
                if active || health.warming.load(Ordering::Acquire) {
                    let watch = watch.get_or_insert_with(|| {
                        Watchdog::new(health.active_epoch.load(Ordering::Acquire))
                    });
                    let time = now(health.start);
                    let probe_at = health.probe_at.load(Ordering::Acquire);
                    let probe_lost = probe_at != 0
                        && health.ack.load(Ordering::Acquire) != 3
                        && time.saturating_sub(probe_at - 1) >= model::PROBE_TIMEOUT_MS;
                    let gate = health.gate.is_open();
                    let epoch = health.gate.epoch();
                    let pump = health.pump.load(Ordering::Acquire);
                    let cause = if health.fault.load(Ordering::Acquire) {
                        1
                    } else if probe_lost {
                        2
                    } else if watch.lost(
                        time,
                        gate,
                        epoch,
                        pump,
                        health.mouse_hook_count.load(Ordering::Acquire),
                        health.raw_count.load(Ordering::Acquire),
                    ) {
                        if !gate || epoch != watch.epoch() {
                            3
                        } else if time.saturating_sub(pump) >= model::PUMP_TIMEOUT_MS {
                            4
                        } else {
                            5
                        }
                    } else {
                        0
                    };
                    if cause != 0 {
                        note_loss(cause);
                        health.terminate(EndReason::Lost);
                    }
                } else {
                    watch = None;
                }
                thread::sleep(Duration::from_millis(10));
            }
        })
        .is_err()
    {
        shared.fail(11);
        return;
    }
    let _ = ready.send(Ok(()));
    let mut raw_check = 0;
    let mut iterations: u64 = 0;
    while !shared.shutdown.load(Ordering::Acquire)
        || shared.drag_order.load(Ordering::Acquire) != 0
        || shared
            .keys
            .iter()
            .chain(shared.buttons.iter())
            .any(|s| s.load(Ordering::Acquire) == 2)
    {
        shared.pump.store(now(shared.start), Ordering::Release);
        // SAFETY: dispatch only this owner thread's messages.
        unsafe {
            let mut message: MSG = std::mem::zeroed();
            for _ in 0..256 {
                if PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) == 0 {
                    break;
                }
                if message.message == WM_QUIT {
                    shared.shutdown.store(true, Ordering::Release);
                    break;
                }
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        iterations = iterations.saturating_add(1);
        // A resync posted by one pump is handled by the next, so a read counts only after two.
        let fresh = iterations >= 2;
        while let Some(record) = shared.ring.pop() {
            process_record(shared, &mut hook, record, monitor);
        }
        if let Some(nonce) = hook.expire_drag_tail(event_time()) {
            shared.expire_drag_tail(nonce);
        }
        if let Some(drag) = &mut drag {
            let mut point = POINT::default();
            let dx = shared.raw_x.swap(0, Ordering::AcqRel);
            let dy = shared.raw_y.swap(0, Ordering::AcqRel);
            if !shared.shutdown.load(Ordering::Acquire)
                && !shared.fault.load(Ordering::Acquire)
                // SAFETY: read-only global pointer position, no foreign window contents.
                && unsafe { GetCursorPos(&mut point) } != 0
            {
                let primary_only = shared.buttons[0].load(Ordering::Acquire) == 1
                    && shared.buttons[1..]
                        .iter()
                        .all(|b| b.load(Ordering::Acquire) == 0);
                match drag.sample(
                    (point.x, point.y),
                    if dx == 0 && dy == 0 {
                        None
                    } else {
                        Some((dx as f64, dy as f64))
                    },
                    primary_only,
                    event_time(),
                ) {
                    Ok(events) => {
                        for event in events {
                            shared.enqueue(None, event);
                        }
                    }
                    Err(_) => shared.fail(12),
                }
            }
        }
        let active = shared.active_id();
        if old_active.is_some() && active.is_none() {
            hook.end(EndReason::Lost, event_time());
            native.release_capture();
            let _ = probe_tx.try_send(None);
            if let Some(drag) = &mut drag {
                for event in drag.retire(event_time()) {
                    shared.enqueue(None, event);
                }
            }
        }
        old_active = active;
        if shared.fault.load(Ordering::Acquire) {
            note_loss(1);
            shared.terminate(EndReason::Lost);
            hook.end(EndReason::Lost, event_time());
            native.release_capture();
            let _ = probe_tx.try_send(None);
            if let Some(p) = pending.take() {
                let _ = p.reply.send(Err(shared.lost("drag")));
            }
            // Narrow fault exception: only the ONE reserved primary's counterpart remains
            // contained. No activation/input delivery/probes occur after terminal failure.
            if shared.drag_order.load(Ordering::Acquire) == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(1));
            continue;
        }
        if let Some(mut p) = pending.take() {
            if shared.generation.load(Ordering::Acquire) != p.generation
                || !shared.gate.is_open()
                || shared.gate.epoch() != p.epoch
                || now(shared.start).saturating_sub(p.started) >= 600
            {
                shared.warming.store(false, Ordering::Release);
                native.release_capture();
                let _ = probe_tx.try_send(None);
                let _ = p.reply.send(Err(PlatformError::Timeout));
                if let Some(drag) = &mut drag {
                    for event in drag.retire(event_time()) {
                        shared.enqueue(None, event);
                    }
                }
            } else if p.drag.is_some()
                && (shared.drag_first.load(Ordering::Acquire) == 4
                    || shared.buttons[0].load(Ordering::Acquire) != 2)
            {
                let _ = p.reply.send(Err(PlatformError::PointerButtonHeld));
                if let Some(drag) = &mut drag {
                    for event in drag.retire(event_time()) {
                        shared.enqueue(None, event);
                    }
                }
            } else if !p.prepared {
                if p.drag.is_some_and(|target| {
                    shared.drag_ready(drag.as_ref().is_some_and(|d| d.ended(target)))
                }) {
                    match prepare_capture(
                        shared,
                        &native,
                        &probe_tx,
                        p.generation,
                        p.epoch,
                        p.point,
                        p.nonce,
                    ) {
                        Ok(()) => {
                            p.prepared = true;
                            pending = Some(p);
                        }
                        Err(e) => {
                            native.release_capture();
                            shared.warming.store(false, Ordering::Release);
                            let _ = p.reply.send(Err(e));
                            if let Some(drag) = &mut drag {
                                for event in drag.retire(event_time()) {
                                    shared.enqueue(None, event);
                                }
                            }
                        }
                    }
                } else {
                    pending = Some(p);
                }
            } else if shared.foreground_refused.load(Ordering::Acquire) {
                // The foreground turned elevated or secure during warm-up and the probe thread
                // sent no probe: refuse now rather than time out. Nothing is owed (nothing was
                // submitted) and nothing is faulted.
                shared.warming.store(false, Ordering::Release);
                native.release_capture();
                let _ = probe_tx.try_send(None);
                let _ = p.reply.send(Err(PlatformError::SecureInput));
                if let Some(drag) = &mut drag {
                    for event in drag.retire(event_time()) {
                        shared.enqueue(None, event);
                    }
                }
            } else if shared.ack.load(Ordering::Acquire) == 3 {
                let (held, mut buttons) = snapshot(shared);
                if p.drag.is_some() {
                    buttons[0] = shared.buttons[0].load(Ordering::Acquire) == 2
                        && shared.drag_order.load(Ordering::Acquire) == 3;
                }
                if buttons
                    .iter()
                    .enumerate()
                    .any(|(i, b)| *b && !(p.drag.is_some() && i == 0))
                    || shared.buttons.iter().enumerate().any(|(i, b)| {
                        b.load(Ordering::Acquire) != 0 && !(p.drag.is_some() && i == 0)
                    })
                {
                    shared.warming.store(false, Ordering::Release);
                    native.release_capture();
                    let _ = probe_tx.try_send(None);
                    let _ = p.reply.send(Err(PlatformError::PointerButtonHeld));
                    if let Some(drag) = &mut drag {
                        for event in drag.retire(event_time()) {
                            shared.enqueue(None, event);
                        }
                    }
                    continue;
                }
                if !native.capture_healthy(p.point) {
                    shared.warming.store(false, Ordering::Release);
                    native.release_capture();
                    let _ = probe_tx.try_send(None);
                    let _ = p.reply.send(Err(error("capture readiness lost")));
                    if let Some(drag) = &mut drag {
                        for event in drag.retire(event_time()) {
                            shared.enqueue(None, event);
                        }
                    }
                    continue;
                }
                let activation = if let Some(target) = p.drag {
                    if !drag.as_ref().is_some_and(|d| d.ended(target)) {
                        Err(PlatformError::NotFound)
                    } else {
                        hook.native_begin_drag(
                            p.id,
                            p.portal,
                            MouseButton::PRIMARY,
                            &held,
                            &buttons,
                            lock_keys(fresh),
                            event_time(),
                        )
                    }
                } else {
                    hook.native_begin(
                        p.id,
                        p.portal,
                        &held,
                        &buttons,
                        lock_keys(fresh),
                        event_time(),
                    )
                };
                match activation {
                    Ok((start, events)) => {
                        clip_point = Some(p.point);
                        shared.active_epoch.store(p.epoch, Ordering::Release);
                        if !model::publish(&shared.generation, p.generation, || {
                            shared.active.store(p.id.0, Ordering::Release)
                        }) {
                            hook.end(EndReason::Lost, event_time());
                            shared.warming.store(false, Ordering::Release);
                            native.release_capture();
                            let _ = probe_tx.try_send(None);
                            let _ = p.reply.send(Err(PlatformError::Timeout));
                            if let Some(drag) = &mut drag {
                                for event in drag.retire(event_time()) {
                                    shared.enqueue(None, event);
                                }
                            }
                            continue;
                        }
                        shared.warming.store(false, Ordering::Release);
                        old_active = Some(p.id.0);
                        for event in events {
                            shared.enqueue(
                                if matches!(event, CaptureEvent::Started { .. }) {
                                    Some((p.generation & !3) | model::ACTIVE)
                                } else {
                                    None
                                },
                                event,
                            );
                        }
                        if shared.generation.load(Ordering::Acquire)
                            != ((p.generation & !3) | model::ACTIVE)
                            || !shared.gate.is_open()
                            || shared.gate.epoch() != p.epoch
                        {
                            note_loss(6);
                            shared.terminate(EndReason::Lost);
                            let _ = p.reply.send(Err(PlatformError::Locked));
                            if let Some(drag) = &mut drag {
                                for event in drag.retire(event_time()) {
                                    shared.enqueue(None, event);
                                }
                            }
                        } else {
                            let _ = p.reply.send(Ok(start));
                        }
                    }
                    Err(e) => {
                        shared.warming.store(false, Ordering::Release);
                        native.release_capture();
                        let _ = probe_tx.try_send(None);
                        let _ = p.reply.send(Err(e));
                        if let Some(drag) = &mut drag {
                            for event in drag.retire(event_time()) {
                                shared.enqueue(None, event);
                            }
                        }
                    }
                }
            } else {
                pending = Some(p);
            }
        }
        while let Ok(command) = commands.try_recv() {
            let (drag_button, command) = match command {
                Command::BeginDrag {
                    id,
                    portal,
                    button,
                    generation,
                    epoch,
                    reply,
                } => (
                    Some(button),
                    Command::Begin {
                        id,
                        portal,
                        generation,
                        epoch,
                        reply,
                    },
                ),
                other => (None, other),
            };
            match command {
                Command::Portals(next, table, reply) => {
                    if let Some(drag) = &mut drag
                        && let Err(e) = drag.set_portals(&next, event_time())
                    {
                        let _ = reply.send(Err(e));
                        continue;
                    }
                    let result = hook.set_portals(next.clone(), event_time()).map(|events| {
                        for event in events {
                            shared.enqueue(None, event);
                        }
                        portals = next;
                        monitors = table;
                    });
                    let _ = reply.send(result);
                }
                Command::Subscribe(reply) => {
                    shared.enqueue(None, CaptureEvent::LockKeys(locks));
                    shared.enqueue(None, CaptureEvent::KeyboardBlinded(false));
                    let _ = reply.send(Ok(()));
                }
                Command::Monitor(on, reply) => {
                    monitor = on;
                    let _ = reply.send(Ok(()));
                }
                Command::End(warp, reply) => {
                    native.release_capture();
                    let _ = probe_tx.try_send(None);
                    hook.end(EndReason::Requested, event_time());
                    if let Some(drag) = &mut drag {
                        for event in drag.retire(event_time()) {
                            shared.enqueue(None, event);
                        }
                    }
                    let result = warp.map_or(Ok(()), |(display, point)| {
                        let rect = monitors
                            .iter()
                            .find(|m| m.0 == display)
                            .ok_or(PlatformError::NotFound)?
                            .1;
                        if !point.x.is_finite()
                            || !point.y.is_finite()
                            || point.x < 0.0
                            || point.y < 0.0
                            || point.x >= f64::from(rect[2] - rect[0])
                            || point.y >= f64::from(rect[3] - rect[1])
                        {
                            return Err(error("invalid capture warp"));
                        }
                        // SAFETY: explicit caller-requested pointer restoration, after unsuppression.
                        if unsafe {
                            SetCursorPos(
                                rect[0] + point.x.round() as i32,
                                rect[1] + point.y.round() as i32,
                            )
                        } == 0
                        {
                            Err(error("capture warp failed"))
                        } else {
                            Ok(())
                        }
                    });
                    let _ = reply.send(result);
                }
                Command::Begin {
                    id,
                    portal,
                    generation,
                    epoch,
                    reply,
                } => {
                    let result = (|| {
                        if pending.is_some() || shared.active_id().is_some() {
                            return Err(error("capture already active"));
                        }
                        let (p, strip) = portals
                            .iter()
                            .find(|p| p.0.id == portal)
                            .ok_or(PlatformError::NotFound)?;
                        let rect = monitors
                            .iter()
                            .find(|m| m.0 == p.display)
                            .ok_or(PlatformError::NotFound)?
                            .1;
                        let mut point = POINT { x: 0, y: 0 };
                        // SAFETY: read-only cursor and its native monitor rectangle, no foreign metadata.
                        unsafe {
                            if GetCursorPos(&mut point) == 0 {
                                return Err(error("capture pointer unavailable"));
                            }
                            if point.x < strip.min.x
                                || point.x >= strip.max.x
                                || point.y < strip.min.y
                                || point.y >= strip.max.y
                            {
                                return Err(PlatformError::NotFound);
                            }
                            let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONULL);
                            let mut info: MONITORINFO = std::mem::zeroed();
                            info.cbSize = size_of::<MONITORINFO>() as u32;
                            if monitor.is_null()
                                || GetMonitorInfoW(monitor, &mut info) == 0
                                || [
                                    info.rcMonitor.left,
                                    info.rcMonitor.top,
                                    info.rcMonitor.right,
                                    info.rcMonitor.bottom,
                                ] != rect
                            {
                                return Err(error("capture display changed"));
                            }
                        }
                        let (_, buttons) = snapshot(shared);
                        if buttons.iter().enumerate().any(|(i, b)| {
                            *b && !(drag_button == Some(MouseButton::PRIMARY) && i == 0)
                        }) {
                            return Err(PlatformError::PointerButtonHeld);
                        }
                        // SAFETY: read-only admission; refuse an existing application-owned clip
                        // rather than overwriting it and later releasing someone else's constraint.
                        unsafe {
                            let mut clip = RECT::default();
                            let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
                            let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
                            if GetClipCursor(&mut clip) == 0
                                || [
                                    i64::from(clip.left),
                                    i64::from(clip.top),
                                    i64::from(clip.right) - i64::from(clip.left),
                                    i64::from(clip.bottom) - i64::from(clip.top),
                                ] != [
                                    left,
                                    top,
                                    GetSystemMetrics(SM_CXVIRTUALSCREEN),
                                    GetSystemMetrics(SM_CYVIRTUALSCREEN),
                                ]
                                .map(i64::from)
                            {
                                return Err(error("pointer clip already owned or unavailable"));
                            }
                        }
                        let probe_nonce = nonce()?;
                        let drag_target = if let Some(button) = drag_button {
                            if button != MouseButton::PRIMARY {
                                return Err(PlatformError::PointerButtonHeld);
                            }
                            let observer = drag.as_mut().ok_or(PlatformError::Unsupported(
                                "drag configuration unavailable",
                            ))?;
                            let target = observer.target(portal)?;
                            let tag = nonce()?;
                            observer.consume(target);
                            let outcome = observer.config.settlement.settle(
                                target.1,
                                tag,
                                || {
                                    hook.protect_drag_tail(id, button, tag)?;
                                    if let Err(e) = shared.reserve_drag(id, generation, tag) {
                                        let _ = hook.cancel_unsubmitted_drag(id);
                                        return Err(e);
                                    }
                                    hook.mark_drag_dispatch(id)
                                },
                                || {
                                    if generation != shared.generation.load(Ordering::Acquire)
                                        || !shared.gate.is_open()
                                        || shared.gate.epoch() != epoch
                                    {
                                        return Err(PlatformError::Locked);
                                    }
                                    if shared.buttons[1..]
                                        .iter()
                                        .any(|b| b.load(Ordering::Acquire) != 0)
                                    {
                                        return Err(PlatformError::PointerButtonHeld);
                                    }
                                    Ok(())
                                },
                            )?;
                            // The callbacks may have run reentrantly during SendInput. Drain
                            // their authoritative order before interpreting the count report.
                            while let Some(record) = shared.ring.pop() {
                                process_record(shared, &mut hook, record, monitor);
                            }
                            let recorded = hook.record_drag_settlement(id, outcome.result);
                            shared.report_drag(outcome.result);
                            if let Some(e) = outcome.error
                                && (outcome.result == DragSettlementResult::Uncertain
                                    || shared.drag_first.load(Ordering::Acquire) != 4)
                            {
                                return Err(e);
                            }
                            if shared.drag_first.load(Ordering::Acquire) == 4 {
                                return Err(PlatformError::PointerButtonHeld);
                            }
                            recorded?;
                            Some(target.1)
                        } else {
                            prepare_capture(
                                shared,
                                &native,
                                &probe_tx,
                                generation,
                                epoch,
                                point,
                                probe_nonce,
                            )?;
                            None
                        };
                        Ok(Pending {
                            id,
                            portal,
                            generation,
                            epoch,
                            started: now(shared.start),
                            point,
                            drag: drag_target,
                            nonce: probe_nonce,
                            prepared: drag_target.is_none(),
                            reply: reply.clone(),
                        })
                    })();
                    match result {
                        Ok(p) => pending = Some(p),
                        Err(e) => {
                            shared.warming.store(false, Ordering::Release);
                            native.release_capture();
                            let _ = reply.send(Err(e));
                            if let Some(drag) = &mut drag {
                                for event in drag.retire(event_time()) {
                                    shared.enqueue(None, event);
                                }
                            }
                        }
                    }
                }
                Command::BeginDrag { .. } => unreachable!("normalized owned command"),
            }
        }
        let next_locks = lock_keys(fresh);
        if locks != next_locks {
            locks = next_locks;
            shared.enqueue(None, CaptureEvent::LockKeys(locks));
        }
        if now(shared.start).saturating_sub(raw_check) >= 100 {
            raw_check = now(shared.start);
            if mouse_target().map_or(true, |target| target != Some(native.hwnd)) {
                shared.fail(13);
                note_loss(1);
                shared.terminate(EndReason::Lost);
            }
            if shared.active_id().is_some()
                && clip_point.is_none_or(|point| !native.capture_healthy(point))
            {
                shared.fail(14);
                note_loss(1);
                shared.terminate(EndReason::Lost);
            }
        }
        thread::sleep(Duration::from_millis(1));
    }
    if LAST_LOSS.load(Ordering::Acquire) == 0 {
        note_loss(8);
    }
    shared.terminate(EndReason::Lost);
    native.cleanup();
    let _ = probe_tx.try_send(None);
    if let Some(p) = pending {
        let _ = p.reply.send(Err(shared.lost("capture")));
    }
    CALLBACK.with(|slot| slot.set(null()));
}

fn probe_thread(shared: Arc<Shared>, commands: mpsc::Receiver<Option<(usize, u64)>>) {
    let mut probe: Option<Probe> = None;
    let mut epoch = 0;
    let mut identity = 0;
    while !shared.owner_done.load(Ordering::Acquire) || probe.is_some() {
        while let Ok(command) = commands.try_recv() {
            match command {
                Some((nonce, next_epoch)) => {
                    probe = Probe::new(nonce, now(shared.start));
                    epoch = next_epoch;
                    identity = nonce;
                }
                None => {
                    if let Some(p) = &mut probe {
                        p.stop(now(shared.start));
                    }
                }
            }
        }
        if let Some(p) = &mut probe {
            let ack = shared.ack.load(Ordering::Acquire);
            for (bit, up) in [(1, false), (2, true)] {
                if ack & bit != 0 {
                    p.acknowledge(
                        shared.nonce.load(Ordering::Acquire),
                        model::F24_SCAN,
                        model::F24_VK,
                        true,
                        up,
                    );
                }
            }
            match p.poll(now(shared.start)) {
                ProbeAction::Pair => {
                    // Decided before anything is recorded or submitted: UIPI drops an injected
                    // probe aimed at an elevated or secure foreground while SendInput still
                    // reports success, so it could never be acknowledged.
                    match model::probe_decision(
                        foreground_class(),
                        probe_phase(&shared),
                        ProbeStage::BeforeSubmit,
                    ) {
                        ProbeDecision::Refuse => {
                            // Warm-up: no probe is sent and the owner refuses the activation.
                            // The probe stays armed (a fresh one is due at once), so if the
                            // capture activated meanwhile its next decision ends it as a loss.
                            shared.foreground_refused.store(true, Ordering::Release);
                            probe = Probe::new(identity, now(shared.start));
                        }
                        ProbeDecision::EndLostElevated => {
                            // Nothing was submitted, so nothing is owed and nothing is faulted.
                            p.submitted(0);
                            p.stop(now(shared.start));
                            end_for_elevated_foreground(&shared);
                        }
                        // A fault needs a submitted probe, so `Fault` cannot occur here.
                        ProbeDecision::Probe | ProbeDecision::Fault => {
                            shared.probe_owed.store(true, Ordering::Release);
                            shared.ack.store(0, Ordering::Release);
                            shared
                                .probe_at
                                .store(now(shared.start).saturating_add(1), Ordering::Release);
                            if !shared.gate.is_open() || shared.gate.epoch() != epoch {
                                p.submitted(0);
                                shared.probe_owed.store(false, Ordering::Release);
                                p.stop(now(shared.start));
                                note_loss(7);
                                shared.terminate(EndReason::Lost);
                            } else {
                                let lost = p.submitted(submit_probe(&shared, identity, false))
                                    == ProbeAction::Lost;
                                shared
                                    .probe_owed
                                    .store(p.cleanup_pending(), Ordering::Release);
                                if lost {
                                    shared.fail(15);
                                    note_loss(1);
                                    shared.terminate(EndReason::Lost);
                                }
                            }
                        }
                    }
                }
                ProbeAction::Lost => {
                    // A submitted probe went unacknowledged. If the foreground has since turned
                    // elevated or secure (it changed between the check and SendInput), UIPI
                    // dropped it: a loss. Otherwise the keyboard hook is gone: a fault.
                    match model::probe_decision(
                        foreground_class(),
                        probe_phase(&shared),
                        ProbeStage::Unacknowledged,
                    ) {
                        ProbeDecision::EndLostElevated => end_for_elevated_foreground(&shared),
                        ProbeDecision::Fault | ProbeDecision::Probe | ProbeDecision::Refuse => {
                            shared.fail(16);
                            note_loss(1);
                            shared.terminate(EndReason::Lost);
                        }
                    }
                }
                ProbeAction::Up => {
                    p.up_submitted(submit_probe(&shared, identity, true));
                    shared
                        .probe_owed
                        .store(p.cleanup_pending(), Ordering::Release);
                }
                ProbeAction::CleanupUnverified => {
                    eprintln!("crosspane capture probe release unverified");
                    shared.probe_owed.store(false, Ordering::Release);
                    probe = None;
                }
                ProbeAction::None => {
                    if shared.owner_done.load(Ordering::Acquire) && !p.cleanup_pending() {
                        probe = None;
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn process_record(shared: &Shared, hook: &mut HookState, record: Record, monitor: bool) {
    if record.kind & 255 == 4 {
        hook.drag_tagged_up(record.data as usize);
        return;
    }
    let at = event_time();
    let decision = if record.kind & 255 == 1 {
        let flags = (record.kind >> 8) as u32;
        hook.queued_key(
            record.captured.then_some(CaptureId(record.id)),
            KeyIn {
                scancode: (record.kind >> 40) as u8,
                extended: flags & LLKHF_EXTENDED != 0,
                vk: record.data as u32,
                up: flags & LLKHF_UP != 0,
                injected: false,
                ours: false,
                at,
            },
        )
    } else {
        let message = (record.kind >> 8) as u32 & 0xffff;
        let data = (record.kind >> 32) as u32;
        let kind = match message {
            WM_LBUTTONDOWN | WM_LBUTTONUP => MouseKind::Button {
                n: 0,
                down: message == WM_LBUTTONDOWN,
            },
            WM_RBUTTONDOWN | WM_RBUTTONUP => MouseKind::Button {
                n: 1,
                down: message == WM_RBUTTONDOWN,
            },
            WM_MBUTTONDOWN | WM_MBUTTONUP => MouseKind::Button {
                n: 2,
                down: message == WM_MBUTTONDOWN,
            },
            WM_XBUTTONDOWN | WM_XBUTTONUP => MouseKind::Button {
                n: if data >> 16 == 1 { 3 } else { 4 },
                down: message == WM_XBUTTONDOWN,
            },
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => MouseKind::Wheel {
                v120: (data >> 16) as i16 as i32,
                horizontal: message == WM_MOUSEHWHEEL,
            },
            _ => MouseKind::Move,
        };
        hook.on_mouse(MouseIn {
            kind,
            pt: (record.data as u32 as i32, (record.data >> 32) as u32 as i32),
            delta: None,
            dragged: false,
            buttons_down: None,
            injected: false,
            ours: false,
            at,
        })
    };
    if monitor && !record.captured && shared.active_id().is_none() {
        shared.enqueue(None, CaptureEvent::LocalActivity { at });
    }
    for event in decision.events {
        if matches!(event, CaptureEvent::Motion { .. }) {
            continue;
        } // raw mouse owns captured motion
        let id = if matches!(
            event,
            CaptureEvent::Key { .. } | CaptureEvent::Button { .. } | CaptureEvent::Scroll { .. }
        ) {
            if !record.captured {
                continue;
            }
            Some(record.token)
        } else {
            None
        };
        if id.is_none() || shared.active_token() == id {
            shared.enqueue(id, event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Sink(Mutex<Vec<CaptureEvent>>);
    impl EventSink<CaptureEvent> for Sink {
        fn send(&self, event: CaptureEvent) {
            lock(&self.0).push(event);
        }
    }
    fn activate(shared: &Shared, id: u64) -> u64 {
        shared.gate.set_session_permits(true);
        shared.gate.set_engine_permits(true);
        shared
            .active_epoch
            .store(shared.gate.epoch(), Ordering::Release);
        let ticket = model::reserve(&shared.generation).unwrap();
        assert!(model::publish(&shared.generation, ticket, || shared
            .active
            .store(id, Ordering::Release)));
        shared.active_token().unwrap()
    }
    #[test]
    fn bounded_callback_ring_preserves_zero_capture_and_never_overwrites() {
        let ring = Ring::new();
        for index in 0..RECORDS {
            assert!(ring.push(Record {
                kind: index as u64,
                data: 0,
                id: 0,
                captured: true,
                token: 5
            }));
        }
        assert!(!ring.push(Record {
            kind: 9999,
            data: 0,
            id: 0,
            captured: false,
            token: 0
        }));
        for index in 0..RECORDS {
            let record = ring.pop().unwrap();
            assert_eq!(record.kind, index as u64);
            assert!(record.captured);
            assert_eq!(record.token, 5);
        }
        assert!(ring.pop().is_none());
    }
    #[test]
    fn zero_id_starts_then_ends_once_and_no_input_follows_abort() {
        let shared = Shared::new(IoGate::new());
        let sink = Arc::new(Sink(Mutex::new(Vec::new())));
        assert!(shared.sink.set(sink.clone()).is_ok());
        let token = activate(&shared, 0);
        shared.enqueue(Some(token), CaptureEvent::Started { id: CaptureId(0) });
        shared.enqueue(
            Some(token),
            CaptureEvent::Motion {
                dx: 1.,
                dy: 0.,
                kind: MotionKind::Unaccelerated,
                at: MonoTime::ZERO,
            },
        );
        shared.terminate(EndReason::Aborted);
        shared.terminate(EndReason::Lost);
        shared.owner_done.store(true, Ordering::Release);
        deliver_events(&shared);
        assert_eq!(
            *lock(&sink.0),
            vec![
                CaptureEvent::Started { id: CaptureId(0) },
                CaptureEvent::Ended {
                    id: CaptureId(0),
                    reason: EndReason::Aborted
                }
            ]
        );
        assert!(shared.active_id().is_none());
        assert!(!shared.end_claim.load(Ordering::Acquire));
    }
    #[test]
    fn reused_capture_id_never_admits_old_queue_generation() {
        let shared = Shared::new(IoGate::new());
        let old = activate(&shared, 7);
        model::cancel(&shared.generation);
        let next = activate(&shared, 7);
        assert_ne!(old, next);
        assert!(!shared.admits(Some(old), Some(next)));
        assert!(shared.admits(Some(next), Some(next)));
    }
    #[test]
    fn closed_or_reopened_gate_immediately_refuses_callback_and_queue_admission() {
        let shared = Shared::new(IoGate::new());
        let token = activate(&shared, 7);
        shared.gate.set_session_permits(false);
        assert!(shared.active_snapshot().is_none());
        assert!(!shared.admits(Some(token), Some(token)));
        shared.gate.set_session_permits(true);
        assert!(shared.active_snapshot().is_none());
    }
    #[test]
    fn bounded_delivery_queue_fails_closed_but_reserves_started_pairing() {
        let shared = Shared::new(IoGate::new());
        for _ in 0..4096 {
            shared.enqueue(None, CaptureEvent::KeyboardBlinded(false));
        }
        shared.enqueue(None, CaptureEvent::KeyboardBlinded(false));
        assert!(shared.fault.load(Ordering::Acquire));
        shared.enqueue(Some(5), CaptureEvent::Started { id: CaptureId(0) });
        assert_eq!(lock(&shared.events).len(), 4097);
    }

    fn reserved_drag() -> Shared {
        let shared = Shared::new(IoGate::new());
        shared.buttons[0].store(1, Ordering::Release);
        let generation = model::reserve(&shared.generation).unwrap();
        shared.reserve_drag(CaptureId(7), generation, 1001).unwrap();
        shared
    }
    #[test]
    fn drag_callback_event_order_exactly_one_up_passes_in_every_send_report_order() {
        for tagged_first in [false, true] {
            for result in [
                DragSettlementResult::Accepted,
                DragSettlementResult::Uncertain,
            ] {
                let shared = reserved_drag();
                assert_eq!(shared.drag_up(true, 999), None);
                assert_eq!(
                    shared.drag_up(tagged_first, if tagged_first { 1001 } else { 0 }),
                    Some(false)
                );
                shared.report_drag(result);
                assert_eq!(
                    shared.drag_first.load(Ordering::Acquire),
                    if tagged_first { 3 } else { 4 }
                );
                assert_eq!(
                    shared.drag_up(!tagged_first, if tagged_first { 0 } else { 1001 }),
                    Some(true)
                );
                assert_eq!(shared.drag_order.load(Ordering::Acquire), 0);
                assert_eq!(shared.drag_up(true, 1001), None);
            }
        }
    }
    #[test]
    fn drag_callback_zero_after_physical_up_finishes_without_impossible_tag_or_capture() {
        let shared = reserved_drag();
        assert_eq!(shared.drag_up(false, 0), Some(false));
        shared.report_drag(DragSettlementResult::KnownZero);
        assert_eq!(shared.drag_order.load(Ordering::Acquire), 0);
        assert_eq!(shared.buttons[0].load(Ordering::Acquire), 0);
        assert!(!shared.drag_ready(true));
        assert_eq!(shared.drag_first.load(Ordering::Acquire), 4);
        let shared = reserved_drag();
        shared.report_drag(DragSettlementResult::KnownZero);
        assert_eq!(shared.buttons[0].load(Ordering::Acquire), 1);
        assert_eq!(shared.drag_up(false, 0), None); // Original local up passes normally.
    }
    #[test]
    fn drag_native_start_requires_observed_tagged_up_actual_end_and_primary_only() {
        let shared = reserved_drag();
        shared.report_drag(DragSettlementResult::Accepted);
        assert!(!shared.drag_ready(true));
        assert_eq!(shared.drag_up(true, 1001), Some(false));
        assert!(!shared.drag_ready(false));
        assert!(shared.drag_ready(true));
        shared.buttons[1].store(1, Ordering::Release);
        assert!(!shared.drag_ready(true));
        shared.buttons[1].store(0, Ordering::Release);
        assert_eq!(shared.drag_up(false, 0), Some(true));
        assert!(!shared.drag_ready(true));
    }
    #[test]
    fn drag_callback_terminal_activation_keeps_counterpart_but_no_post_ended_input() {
        for tagged_first in [false, true] {
            let shared = reserved_drag();
            shared.drag_up(tagged_first, if tagged_first { 1001 } else { 0 });
            shared.report_drag(DragSettlementResult::Uncertain);
            shared.terminate(EndReason::Aborted);
            shared.shutdown.store(true, Ordering::Release);
            assert!(shared.active_id().is_none());
            assert!(!shared.end_pending.load(Ordering::Acquire));
            assert_ne!(shared.drag_order.load(Ordering::Acquire), 0); // Owner lifetime retained.
            assert_eq!(
                shared.drag_up(!tagged_first, if tagged_first { 0 } else { 1001 }),
                Some(true)
            );
            assert_eq!(shared.drag_order.load(Ordering::Acquire), 0);
            assert!(lock(&shared.events).is_empty());
        }
    }

    #[test]
    fn own_token_integrity_reads_and_agrees_with_a_process_query() {
        let own = own_integrity().expect("this process's own token integrity is readable");
        // SAFETY: reads only this process's id.
        let pid = unsafe { GetCurrentProcessId() };
        assert!((0x1000..=0x5000).contains(&own), "implausible RID {own:#x}");
        assert_eq!(process_integrity(pid), Some(own));
        // Our own windows never read as a higher integrity than ours.
        assert_ne!(
            model::classify_foreground(true, process_integrity(pid), Some(own)),
            ForegroundClass::Higher
        );
    }
    #[test]
    fn a_process_that_cannot_be_opened_reads_as_unknown_not_as_admitted() {
        // Pid 0 is the idle process and cannot be opened; an unreadable foreground fails closed.
        assert_eq!(process_integrity(0), None);
        assert_eq!(
            model::classify_foreground(true, process_integrity(0), own_integrity()),
            ForegroundClass::Unknown
        );
    }
    #[test]
    fn the_foreground_read_is_total_and_reads_no_content() {
        // Whatever is in front on this machine, the read completes with one of the four classes.
        assert!(matches!(
            foreground_class(),
            ForegroundClass::Absent
                | ForegroundClass::Unknown
                | ForegroundClass::Higher
                | ForegroundClass::LowerOrEqual
        ));
    }
    #[test]
    fn an_elevated_foreground_ends_a_live_capture_as_lost_and_never_as_a_fault() {
        let shared = Shared::new(IoGate::new());
        assert_eq!(probe_phase(&shared), ProbePhase::Active);
        // No capture is live: a stale decision changes and records nothing.
        let idle = shared.generation.load(Ordering::Acquire);
        end_for_elevated_foreground(&shared);
        assert_eq!(shared.generation.load(Ordering::Acquire), idle);
        assert!(!shared.end_pending.load(Ordering::Acquire));
        assert!(!shared.fault.load(Ordering::Acquire));
        // A warm-up is cancelled, not faulted.
        shared.warming.store(true, Ordering::Release);
        assert_eq!(probe_phase(&shared), ProbePhase::Warming);
        let pending = model::reserve(&shared.generation).unwrap();
        end_for_elevated_foreground(&shared);
        assert_ne!(shared.generation.load(Ordering::Acquire), pending);
        assert_eq!(shared.generation.load(Ordering::Acquire) & 3, model::IDLE);
        assert!(!shared.warming.load(Ordering::Acquire));
        assert!(!shared.fault.load(Ordering::Acquire));
        // An active capture ends as Lost with the elevated label, and the backend stays usable.
        activate(&shared, 3);
        end_for_elevated_foreground(&shared);
        assert!(shared.active_id().is_none());
        assert!(shared.end_pending.load(Ordering::Acquire));
        assert_eq!(shared.terminal.load(Ordering::Acquire), 3);
        assert_eq!(shared.reason.load(Ordering::Acquire), 2);
        assert!(!shared.fault.load(Ordering::Acquire));
        assert!(!is_dead(&shared));
        assert_eq!(last_loss(), LOSS_CAUSES[usize::from(LOSS_ELEVATED)]);
        assert_eq!(last_loss(), "elevated foreground: input cannot be verified");
    }

    fn limited() -> bool {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            Security::{
                GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
                TokenElevationType, TokenElevationTypeLimited,
            },
            System::Threading::{GetCurrentProcess, OpenProcessToken},
        };
        let mut token = null_mut();
        // SAFETY: read-only queries of this fixture process's token with exact local buffers.
        unsafe {
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let mut elevation = TOKEN_ELEVATION::default();
            let mut kind = 0_i32;
            let mut bytes = 0;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut bytes,
            ) != 0
                && GetTokenInformation(
                    token,
                    TokenElevationType,
                    (&mut kind as *mut i32).cast(),
                    size_of::<i32>() as u32,
                    &mut bytes,
                ) != 0;
            CloseHandle(token);
            ok && elevation.TokenIsElevated == 0 && kind == TokenElevationTypeLimited
        }
    }
    struct Fixture {
        window: HWND,
        class: Vec<u16>,
        instance: windows_sys::Win32::Foundation::HINSTANCE,
    }
    impl Fixture {
        fn create(point: POINT) -> Self {
            // SAFETY: this test registers and creates ONLY its own bounded fixture window.
            unsafe {
                let instance = GetModuleHandleW(null());
                let class: Vec<_> = "CrosspaneCaptureProbeFixture"
                    .encode_utf16()
                    .chain(Some(0))
                    .collect();
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(DefWindowProcW),
                    hInstance: instance,
                    lpszClassName: class.as_ptr(),
                    hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                    ..std::mem::zeroed()
                };
                assert_ne!(RegisterClassW(&wc), 0);
                let window = CreateWindowExW(
                    WS_EX_TOOLWINDOW,
                    class.as_ptr(),
                    class.as_ptr(),
                    WS_POPUP,
                    point.x - 319,
                    point.y - 100,
                    320,
                    200,
                    null_mut(),
                    null_mut(),
                    instance,
                    null(),
                );
                assert!(!window.is_null());
                Self {
                    window,
                    class,
                    instance,
                }
            }
        }
        fn owned_foreground(&self) -> bool {
            let mut pid = 0;
            // SAFETY: authenticated own HWND/PID comparison only; no foreign title/content read.
            unsafe {
                GetWindowThreadProcessId(self.window, &mut pid);
                GetForegroundWindow() == self.window
                    && pid == windows_sys::Win32::System::Threading::GetCurrentProcessId()
            }
        }
        fn pump(&self) {
            // SAFETY: dispatch only messages on this fixture's own thread.
            unsafe {
                let mut message: MSG = std::mem::zeroed();
                for _ in 0..128 {
                    if PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) == 0 {
                        break;
                    }
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // SAFETY: test-owned fixture/class only, never a foreign process/window.
            unsafe {
                DestroyWindow(self.window);
                UnregisterClassW(self.class.as_ptr(), self.instance);
            }
        }
    }
    struct ProbeGuard(Arc<AtomicBool>);
    impl Drop for ProbeGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    #[test]
    #[ignore = "Limited owned fixture only: win-gui --ignored --exact; never elevated cargo"]
    fn limited_owned_capture_begin_end_drop_cleanup() {
        assert!(
            limited(),
            "Limited token required BEFORE windows/hooks/input"
        );
        let done = Arc::new(AtomicBool::new(false));
        let watch: Arc<OnceLock<Arc<Shared>>> = Arc::new(OnceLock::new());
        let abort_done = done.clone();
        let abort_watch = watch.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(3));
            if !abort_done.load(Ordering::Acquire)
                && let Some(shared) = abort_watch.get()
            {
                shared.terminate(EndReason::Aborted);
                shared.shutdown.store(true, Ordering::Release);
            }
        });
        let kill_done = done.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(4500));
            if !kill_done.load(Ordering::Acquire) {
                eprintln!("capture probe independent deadline; cleanup NOT verified");
                // SAFETY: terminate ONLY this owned fixture process; OS releases its hook/window
                // handles. This independent fallback does not rely on a stuck ungrab/API thread.
                unsafe {
                    windows_sys::Win32::System::Threading::ExitProcess(124);
                }
            }
        });
        let _guard = ProbeGuard(done);
        // SAFETY: own thread DPI context; read-only pointer/monitor/status before fixture mutation.
        let (info, point, baseline) = unsafe {
            assert!(
                !windows_sys::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(
                    windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2
                )
                .is_null()
            );
            for vk in [0x10, 0x11, 0x12, 0x5b, 0x5c, model::F24_VK as i32] {
                assert!(
                    GetAsyncKeyState(vk) >= 0,
                    "held modifier/probe key refuses fixture"
                );
            }
            let mut cursor = POINT::default();
            assert_ne!(GetCursorPos(&mut cursor), 0);
            let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONULL);
            assert!(!monitor.is_null());
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = size_of::<MONITORINFO>() as u32;
            assert_ne!(GetMonitorInfoW(monitor, &mut info), 0);
            let mut clip = RECT::default();
            assert_ne!(GetClipCursor(&mut clip), 0);
            let virtual_rect = [
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            ];
            assert_eq!(
                [
                    clip.left,
                    clip.top,
                    clip.right - clip.left,
                    clip.bottom - clip.top
                ],
                virtual_rect,
                "refuse pre-existing pointer clip"
            );
            let point = POINT {
                x: info.rcMonitor.right - 1,
                y: info.rcMonitor.top + (info.rcMonitor.bottom - info.rcMonitor.top) / 2,
            };
            (info, point, [clip.left, clip.top, clip.right, clip.bottom])
        };
        let fixture = Fixture::create(point);
        // SAFETY: show/activate ONLY our fixture, no fallback forcing/ALT/foreign identity.
        unsafe {
            ShowWindow(fixture.window, SW_SHOW);
            SetForegroundWindow(fixture.window);
        }
        let until = Instant::now() + Duration::from_millis(200);
        while !fixture.owned_foreground() && Instant::now() < until {
            fixture.pump();
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            fixture.owned_foreground(),
            "fixture foreground unavailable; no input submitted"
        );
        // SAFETY: pointer placement admitted only immediately after verified own foreground.
        assert_ne!(unsafe { SetCursorPos(point.x, point.y) }, 0);
        let probe = MonitorProbe {
            device_path: "owned-probe-monitor".into(),
            name: "own monitor".into(),
            rc_monitor: [
                info.rcMonitor.left,
                info.rcMonitor.top,
                info.rcMonitor.right,
                info.rcMonitor.bottom,
            ],
            rc_work: [
                info.rcWork.left,
                info.rcWork.top,
                info.rcWork.right,
                info.rcWork.bottom,
            ],
            primary: true,
            // SAFETY: read-only DPI of the authenticated owned fixture.
            dpi: unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForWindow(fixture.window) },
            refresh_millihz: 60_000,
            edid: None,
            twin: false,
            quarter_turns: 0,
        };
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        let mut backend = WindowsCapture::new(gate, &[probe], &mut DisplayIds::default()).unwrap();
        backend
            .shared
            .fixture
            .store(fixture.window as usize, Ordering::Release);
        assert!(watch.set(backend.shared.clone()).is_ok());
        let display = backend.monitors[0].0;
        let height = f64::from(info.rcMonitor.bottom - info.rcMonitor.top);
        backend
            .set_portals(&[CapturePortal {
                id: PortalId(1),
                display,
                edge: Edge::Right,
                from: 0.,
                to: height,
            }])
            .unwrap();
        let (tx, rx) = mpsc::channel();
        backend
            .subscribe(Arc::new(move |event| {
                let _ = tx.send(event);
            }))
            .unwrap();
        assert!(fixture.owned_foreground());
        let began = Instant::now();
        backend.begin(CaptureId(0), PortalId(1)).unwrap();
        assert_eq!(backend.shared.active_id(), Some(0));
        assert_ne!(backend.shared.keyboard.load(Ordering::Acquire), 0);
        assert_ne!(backend.shared.mouse.load(Ordering::Acquire), 0);
        assert_eq!(
            mouse_target().unwrap().map(|w| w as usize),
            Some(backend.shared.window.load(Ordering::Acquire))
        );
        let owned_cursor = backend.shared.cursor.load(Ordering::Acquire);
        // SAFETY: read-only proof of our clip/cursor, no screen/window content acquisition.
        unsafe {
            let mut clip = RECT::default();
            assert_ne!(GetClipCursor(&mut clip), 0);
            assert_eq!(
                [clip.left, clip.top, clip.right, clip.bottom],
                [point.x, point.y, point.x + 1, point.y + 1]
            );
            let mut cursor: CURSORINFO = std::mem::zeroed();
            cursor.cbSize = size_of::<CURSORINFO>() as u32;
            assert_ne!(GetCursorInfo(&mut cursor), 0);
            assert_eq!(cursor.hCursor as usize, owned_cursor);
        }
        println!(
            "Limited fixture capture active: hooks=2 mouse-raw=owned clip=1x1 cursor=owned-transparent nonce-ack=both"
        );
        backend.end(None).unwrap();
        assert!(backend.shared.active_id().is_none());
        assert!(began.elapsed() < Duration::from_secs(5));
        // SAFETY: verify native release/status without posting any additional input.
        unsafe {
            let mut clip = RECT::default();
            assert_ne!(GetClipCursor(&mut clip), 0);
            assert_eq!([clip.left, clip.top, clip.right, clip.bottom], baseline);
            let mut cursor: CURSORINFO = std::mem::zeroed();
            cursor.cbSize = size_of::<CURSORINFO>() as u32;
            assert_ne!(GetCursorInfo(&mut cursor), 0);
            assert_ne!(cursor.hCursor as usize, owned_cursor);
        }
        assert_eq!(model::key_transition(0, false, false), (1, false));
        let shared = backend.shared.clone();
        drop(backend);
        let until = Instant::now() + Duration::from_secs(1);
        while !shared.owner_done.load(Ordering::Acquire) && Instant::now() < until {
            fixture.pump();
            thread::sleep(Duration::from_millis(5));
        }
        assert!(shared.owner_done.load(Ordering::Acquire));
        assert_eq!(shared.keyboard.load(Ordering::Acquire), 0);
        assert_eq!(shared.mouse.load(Ordering::Acquire), 0);
        assert!(mouse_target().unwrap().is_none());
        assert!(!shared.probe_owed.load(Ordering::Acquire));
        assert!(shared.cleanup_verified.load(Ordering::Acquire));
        // SAFETY: status checks only, never attempt a generic modifier cleanup injection.
        unsafe {
            for vk in [0x10, 0x11, 0x12, 0x5b, 0x5c, model::F24_VK as i32] {
                assert!(GetAsyncKeyState(vk) >= 0);
            }
        }
        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, CaptureEvent::Started { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, CaptureEvent::Ended { .. }))
                .count(),
            1
        );
        println!(
            "End verified clip/cursor restored and fresh-key decision passes; Drop verified hooks=0 mouse-raw=0; modifier/F24 status up (zero-query ambiguity remains)"
        );
    }

    /// Pure fixture: `rc_work` is the monitor minus a 40-pixel bottom strip.
    fn refresh_probe(path: &str, rc: [i32; 4], primary: bool, twin: bool) -> MonitorProbe {
        MonitorProbe {
            device_path: path.into(),
            name: path.into(),
            rc_monitor: rc,
            rc_work: [rc[0], rc[1], rc[2], rc[3] - 40],
            primary,
            dpi: 96,
            refresh_millihz: 60_000,
            edid: None,
            twin,
            quarter_turns: 0,
        }
    }

    fn portal_on(display: DisplayId, edge: Edge, from: f64, to: f64) -> CapturePortal {
        CapturePortal {
            id: PortalId(1),
            display,
            edge,
            from,
            to,
        }
    }

    fn refused_portal(portal: CapturePortal, monitors: &[(DisplayId, [i32; 4])]) -> bool {
        matches!(
            portal_strips(&[portal], monitors),
            Err(PlatformError::Backend(m)) if m == "invalid capture portal"
        )
    }

    #[test]
    fn capture_refresh_table_matches_construction_rules() {
        let left = refresh_probe("display-a", [0, 0, 1920, 1080], true, false);
        let right = refresh_probe("display-b", [1920, 0, 3360, 1440], false, false);
        // A twin mirrors a physical display and never enters the capture table.
        let twin = refresh_probe("twin-a", [0, 0, 1920, 1080], false, true);
        let mut ids = DisplayIds::default();
        let table = monitor_table(&[right.clone(), twin, left.clone()], &mut ids).unwrap();
        // Device-path order, with the allocator's stable ID for each path.
        let mut expected = DisplayIds::default();
        let a = expected.assign("display-a").unwrap();
        let b = expected.assign("display-b").unwrap();
        assert_eq!(table, vec![(a, left.rc_monitor), (b, right.rc_monitor)]);
        // Refusals match construction: twins alone, invalid primaries, unrepresentable widths.
        let twin_only = [refresh_probe("twin-a", [0, 0, 1920, 1080], true, true)];
        assert!(matches!(
            monitor_table(&twin_only, &mut DisplayIds::default()),
            Err(PlatformError::Backend(m)) if m == "no capture display"
        ));
        let two_primaries = [
            left.clone(),
            refresh_probe("display-b", [1920, 0, 3360, 1440], true, false),
        ];
        assert!(matches!(
            monitor_table(&two_primaries, &mut DisplayIds::default()),
            Err(PlatformError::Backend(m)) if m == "invalid capture display layout"
        ));
        let no_primary = [refresh_probe("display-a", [0, 0, 1920, 1080], false, false)];
        assert!(matches!(
            monitor_table(&no_primary, &mut DisplayIds::default()),
            Err(PlatformError::Backend(m)) if m == "invalid capture display layout"
        ));
        let wide = [refresh_probe(
            "display-w",
            [-2_000_000_000, 0, 2_000_000_000, 1080],
            true,
            false,
        )];
        assert!(matches!(
            monitor_table(&wide, &mut DisplayIds::default()),
            Err(PlatformError::Backend(m)) if m == "unrepresentable capture monitor"
        ));
    }

    #[test]
    fn capture_refresh_hotplugged_display_gets_strips() {
        let primary = refresh_probe("display-a", [0, 0, 1920, 1080], true, false);
        let mut ids = DisplayIds::default();
        let before = monitor_table(std::slice::from_ref(&primary), &mut ids).unwrap();
        // A display connected after startup joins the same allocator and keeps display-a's ID.
        let added = refresh_probe("display-c", [1920, 0, 3840, 1440], false, false);
        let table = monitor_table(&[primary, added.clone()], &mut ids).unwrap();
        let new_id = ids.assign("display-c").unwrap();
        assert_eq!(table[0].0, before[0].0);
        assert!(table.contains(&(new_id, added.rc_monitor)));
        let strips = portal_strips(&[portal_on(new_id, Edge::Right, 0.0, 100.0)], &table).unwrap();
        assert_eq!(strips.len(), 1);
        assert_eq!(strips[0].1.min.x, added.rc_monitor[2] - 1);
        assert_eq!(strips[0].1.max.x, added.rc_monitor[2]);
    }

    #[test]
    fn capture_refresh_removed_display_is_not_found() {
        let primary = refresh_probe("display-a", [0, 0, 1920, 1080], true, false);
        let secondary = refresh_probe("display-b", [1920, 0, 3360, 1440], false, false);
        let mut ids = DisplayIds::default();
        monitor_table(&[primary.clone(), secondary], &mut ids).unwrap();
        let removed = ids.assign("display-b").unwrap();
        // The refresh no longer reports display-b; the allocator keeps its ID for that path.
        let table = monitor_table(std::slice::from_ref(&primary), &mut ids).unwrap();
        assert!(table.iter().all(|(id, _)| *id != removed));
        // A portal still naming the removed display is NotFound; the surviving display still maps.
        let stale = portal_on(removed, Edge::Left, 0.0, 10.0);
        assert!(matches!(
            portal_strips(&[stale], &table),
            Err(PlatformError::NotFound)
        ));
        let kept = portal_on(table[0].0, Edge::Left, 0.0, 10.0);
        assert!(portal_strips(&[kept], &table).is_ok());
    }

    #[test]
    fn capture_refresh_resolution_change_moves_strip() {
        let probe = refresh_probe("display-a", [0, 0, 1920, 1080], true, false);
        let mut ids = DisplayIds::default();
        let before = monitor_table(std::slice::from_ref(&probe), &mut ids).unwrap();
        let right = portal_on(before[0].0, Edge::Right, 0.0, 100.0);
        let bottom = portal_on(before[0].0, Edge::Bottom, 0.0, 100.0);
        let old_right = portal_strips(&[right], &before).unwrap();
        assert_eq!(old_right[0].1.min.x, 1919);
        // The same device path at a new mode keeps its ID; both edge strips follow the new size.
        let grown = refresh_probe("display-a", [0, 0, 2560, 1440], true, false);
        let after = monitor_table(&[grown], &mut ids).unwrap();
        assert_eq!(after[0].0, before[0].0);
        let new_right = portal_strips(&[right], &after).unwrap();
        assert_eq!(new_right[0].1.min.x, 2559);
        assert_eq!(new_right[0].1.max.x, 2560);
        let new_bottom = portal_strips(&[bottom], &after).unwrap();
        assert_eq!(new_bottom[0].1.min.y, 1439);
        assert_eq!(new_bottom[0].1.max.y, 1440);
    }

    #[test]
    fn capture_refresh_invalid_portal_refused() {
        let probe = refresh_probe("display-a", [0, 0, 1920, 1080], true, false);
        let mut ids = DisplayIds::default();
        let table = monitor_table(std::slice::from_ref(&probe), &mut ids).unwrap();
        let id = table[0].0;
        // Empty, reversed, non-finite and negative stretches are refused.
        assert!(refused_portal(
            portal_on(id, Edge::Right, 10.0, 10.0),
            &table
        ));
        assert!(refused_portal(
            portal_on(id, Edge::Right, 20.0, 10.0),
            &table
        ));
        assert!(refused_portal(
            portal_on(id, Edge::Right, f64::NAN, 10.0),
            &table
        ));
        assert!(refused_portal(
            portal_on(id, Edge::Right, -1.0, 10.0),
            &table
        ));
        // A stretch past the display's length (its height for Left and Right) is refused.
        assert!(refused_portal(
            portal_on(id, Edge::Right, 0.0, 1081.0),
            &table
        ));
        // Resolution shrinks: a portal valid on the old table is refused on the new one.
        let shrunk = refresh_probe("display-a", [0, 0, 1280, 720], true, false);
        let shrunk_table = monitor_table(&[shrunk], &mut ids).unwrap();
        let old_valid = portal_on(id, Edge::Right, 0.0, 1000.0);
        assert!(portal_strips(&[old_valid], &table).is_ok());
        assert!(refused_portal(old_valid, &shrunk_table));
        // An unknown display is NotFound rather than an invalid portal.
        let unknown = portal_on(DisplayId(id.0.wrapping_add(1)), Edge::Right, 0.0, 10.0);
        assert!(matches!(
            portal_strips(&[unknown], &table),
            Err(PlatformError::NotFound)
        ));
    }

    #[test]
    fn capture_refresh_skipped_for_empty_portals() {
        let probe = refresh_probe("display-a", [0, 0, 1920, 1080], true, false);
        let mut ids = DisplayIds::default();
        let table = monitor_table(std::slice::from_ref(&probe), &mut ids).unwrap();
        // A CLEAR reads no display: the empty set maps to no strips against the kept table.
        assert!(!portals_need_refresh(&[]));
        assert!(portal_strips(&[], &table).unwrap().is_empty());
        // Any non-empty set still refreshes before its strips are mapped.
        let armed = portal_on(table[0].0, Edge::Right, 0.0, 100.0);
        assert!(portals_need_refresh(&[armed]));
    }

    #[test]
    fn capture_locks_from_states_decodes_toggle_bit() {
        let keys = |caps: bool, num: bool, scroll: bool| LockKeys {
            caps_lock: Some(caps),
            num_lock: Some(num),
            scroll_lock: Some(scroll),
        };
        // Bit 0 is the toggle state, decoded per key.
        assert_eq!(locks_from_states([1, 0, 0]), keys(true, false, false));
        assert_eq!(locks_from_states([0, 1, 0]), keys(false, true, false));
        assert_eq!(locks_from_states([0, 0, 1]), keys(false, false, true));
        assert_eq!(locks_from_states([0, 0, 0]), keys(false, false, false));
        // 0x8000 is the high "down" bit alone and never sets a lock; 0x8001 adds the toggle bit.
        let down = 0x8000_u16 as i16;
        assert_eq!(
            locks_from_states([down, down, down]),
            keys(false, false, false)
        );
        assert_eq!(
            locks_from_states([down | 1, down, down]),
            keys(true, false, false)
        );
        // Without a fresh pump read nothing is known; this path calls no native API.
        assert_eq!(lock_keys(false), LockKeys::default());
    }

    #[test]
    fn capture_health_reports_a_fault_or_an_ended_owner_and_never_keeps_the_state_alive() {
        let serving = Arc::new(Shared::new(IoGate::new()));
        let health = CaptureHealth(Arc::downgrade(&serving));
        assert!(!is_dead(&serving));
        assert_eq!(health.lost(), None);
        // A fault is terminal and names its first site.
        serving.fail(13);
        serving.fail(2);
        assert!(is_dead(&serving));
        assert_eq!(health.lost(), Some("mouse raw input registration changed"));
        // An owner that ended without a fault says so.
        let ended = Arc::new(Shared::new(IoGate::new()));
        let ended_health = CaptureHealth(Arc::downgrade(&ended));
        ended.owner_done.store(true, Ordering::Release);
        assert!(is_dead(&ended));
        assert_eq!(ended_health.lost(), Some("owner thread ended"));
        // The handle is weak: once the state is gone the backend counts as released.
        assert_eq!(Arc::strong_count(&serving), 1);
        drop(serving);
        assert_eq!(health.lost(), Some("backend released"));
    }

    #[test]
    fn waiting_for_the_owner_slot_returns_once_the_last_holder_drops() {
        // Dropping the state releases the process-wide slot, as the last thread of a dead backend
        // does. Another test dropping a `Shared` can only release it earlier, never hold it.
        OWNER.store(true, Ordering::Release);
        let shared = Arc::new(Shared::new(IoGate::new()));
        let holder = shared.clone();
        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            drop(holder);
        });
        drop(shared);
        assert!(wait_owner_released(Duration::from_secs(5)));
        releaser.join().unwrap();
        assert!(!OWNER.load(Ordering::Acquire));
    }
}
