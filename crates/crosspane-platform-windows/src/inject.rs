//! Windows physical input, sharing one ledger and SendInput source across key/pointer handles.
//! `[E]` UIPI may suppress input even when SendInput succeeds; results mean submitted only.
//! <https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput>
//! `[P]` P9d: read-only foreground/token checks cannot atomically bind SendInput to a HWND.
//! Cleanup ups intentionally reach the current foreground. No activation or UIPI bypass is used.
//! `[P]` An owned WinEvent/message/repeat thread detects foreground ABA and gate epochs.
//! `[U]` Event delivery can lag; fresh foreground/PID/start/token checks also precede submissions.
//! `[U]` Native-call stalls and arbitrary application/IME delivery remain outside submission proof.

#![allow(unsafe_code)]

use crate::model::{
    geometry::{DisplayIds, MonitorProbe, displays},
    hook::DragSettlementResult,
    inject::{
        Driver, Foreground, InjectionPort, LocalSettlementOutcome, Packet, RepeatSettings,
        ScrollSettings, absolute_move,
    },
};
use crosspane_platform::{
    IoGate, PlatformError,
    inject::{KeyInjector, PointerInjector},
};
use crosspane_types::{
    geom::PointDevice,
    hid::{HidUsage, MouseButton},
    id::DisplayId,
    input::{LockKeys, ScrollDelta},
};
use std::{
    cell::RefCell,
    mem::size_of,
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, FILETIME, HANDLE},
    Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW},
    Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, IsValidSid,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel,
    },
    System::Threading::{
        GetCurrentProcess, GetProcessTimes, OpenProcess, OpenProcessToken,
        PROCESS_QUERY_LIMITED_INFORMATION,
    },
    UI::{
        Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
        HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext},
        Input::KeyboardAndMouse::*,
        WindowsAndMessaging::*,
    },
};

fn unavailable() -> PlatformError {
    PlatformError::Backend("Windows input submission unavailable".into())
}
fn lock<T>(m: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>, PlatformError> {
    m.lock().map_err(|_| unavailable())
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: this RAII value owns exactly the successful OpenProcess/OpenProcessToken handle.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn integrity(process: HANDLE) -> Result<u32, PlatformError> {
    let mut token = null_mut();
    // SAFETY: read-only query, valid process handle and initialized output storage.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(PlatformError::SecureInput);
    }
    let token = Handle(token);
    let mut needed = 0;
    // SAFETY: zero-length size query writes only needed.
    unsafe {
        GetTokenInformation(token.0, TokenIntegrityLevel, null_mut(), 0, &mut needed);
    }
    if needed < size_of::<TOKEN_MANDATORY_LABEL>() as u32 || needed > 4096 {
        return Err(PlatformError::SecureInput);
    }
    // usize storage provides pointer alignment for TOKEN_MANDATORY_LABEL and its SID.
    let mut buffer = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
    // SAFETY: aligned buffer has at least needed bytes and remains alive through SID inspection.
    unsafe {
        if GetTokenInformation(
            token.0,
            TokenIntegrityLevel,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        ) == 0
        {
            return Err(PlatformError::SecureInput);
        }
        let label = &*(buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>());
        let sid = label.Label.Sid;
        let start = buffer.as_ptr() as usize;
        let end = start + buffer.len() * size_of::<usize>();
        let address = sid as usize;
        if address < start || address.checked_add(8).is_none_or(|a| a > end) {
            return Err(PlatformError::SecureInput);
        }
        let count = *GetSidSubAuthorityCount(sid);
        if count == 0
            || address
                .checked_add(8 + usize::from(count) * 4)
                .is_none_or(|a| a > end)
            || IsValidSid(sid) == 0
        {
            return Err(PlatformError::SecureInput);
        }
        Ok(*GetSidSubAuthority(sid, u32::from(count - 1)))
    }
}

fn foreground(generation: &AtomicU64) -> Result<Foreground, PlatformError> {
    let before = generation.load(Ordering::Acquire);
    // SAFETY: read-only Win32 identity queries; no titles or content are acquired.
    unsafe {
        let window = GetForegroundWindow();
        if window.is_null() {
            return Err(PlatformError::SecureInput);
        }
        let mut pid = 0;
        let thread = GetWindowThreadProcessId(window, &mut pid);
        if pid == 0 || thread == 0 {
            return Err(PlatformError::SecureInput);
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return Err(PlatformError::SecureInput);
        }
        let process = Handle(process);
        let mut created = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        if GetProcessTimes(process.0, &mut created, &mut exit, &mut kernel, &mut user) == 0 {
            return Err(PlatformError::SecureInput);
        }
        let rid = integrity(process.0)?;
        if rid > integrity(GetCurrentProcess())? {
            return Err(PlatformError::SecureInput);
        }
        let mut after_pid = 0;
        if GetForegroundWindow() != window
            || GetWindowThreadProcessId(window, &mut after_pid) != thread
            || after_pid != pid
            || generation.load(Ordering::Acquire) != before
        {
            return Err(PlatformError::SecureInput);
        }
        Ok(Foreground {
            window: window as u64,
            process: pid,
            thread,
            born: (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime),
            generation: before,
            integrity: rid,
        })
    }
}

struct NativePort {
    gate: Arc<IoGate>,
    generation: Arc<AtomicU64>,
    expected: Option<Foreground>,
    expected_gate_epoch: u64,
    cancel: Arc<AtomicU64>,
    repeat_fence: Option<u64>,
    repeat_owner: u64,
    tag: usize,
    probes: Vec<MonitorProbe>,
    ids: DisplayIds,
    refresh: MonitorRefresh,
    #[cfg(test)]
    owned_fixture: Option<usize>,
    #[cfg(test)]
    submitted: usize,
    #[cfg(test)]
    observer: Option<Box<dyn FnMut() -> Result<Foreground, PlatformError> + Send>>,
    #[cfg(test)]
    sender: Option<Box<dyn FnMut(INPUT) -> u32 + Send>>,
    #[cfg(test)]
    tainted: Option<Arc<AtomicBool>>,
}
impl NativePort {
    fn observe(&mut self) -> Result<Foreground, PlatformError> {
        #[cfg(test)]
        if self
            .tainted
            .as_ref()
            .is_some_and(|t| t.load(Ordering::Acquire))
        {
            return Err(PlatformError::SecureInput);
        }
        #[cfg(test)]
        if let Some(observer) = &mut self.observer {
            return observer();
        }
        foreground(&self.generation)
    }
}
impl InjectionPort for NativePort {
    fn foreground(&mut self) -> Result<Foreground, PlatformError> {
        let observed = self.observe()?;
        #[cfg(test)]
        if self
            .owned_fixture
            .is_some_and(|h| observed.window != h as u64)
        {
            return Err(PlatformError::SecureInput);
        }
        self.expected = Some(observed);
        self.expected_gate_epoch = self.gate.epoch();
        Ok(observed)
    }
    fn submit(&mut self, packet: Packet) -> Result<(), PlatformError> {
        #[cfg(test)]
        // SAFETY: test-only containment guard, including ups, reads only the foreground HWND.
        if self.owned_fixture.is_some_and(|h| {
            // SAFETY: test containment reads only the foreground HWND.
            (unsafe { GetForegroundWindow() }) as usize != h
        }) {
            return Err(PlatformError::SecureInput);
        }
        let release = matches!(
            packet,
            Packet::Key { down: false, .. } | Packet::Button { down: false, .. }
        );
        if !release {
            let epoch = self.gate.epoch();
            if !self.gate.is_open() || epoch != self.expected_gate_epoch {
                return Err(PlatformError::Locked);
            }
            if self
                .repeat_fence
                .is_some_and(|c| self.cancel.load(Ordering::Acquire) != c)
            {
                return Err(PlatformError::Locked);
            }
            #[cfg(test)]
            if self
                .tainted
                .as_ref()
                .is_some_and(|t| t.load(Ordering::Acquire))
            {
                return Err(PlatformError::SecureInput);
            }
            let observed = self.observe()?;
            if self.expected != Some(observed) {
                return Err(PlatformError::SecureInput);
            }
            if !self.gate.is_open() || self.gate.epoch() != epoch {
                return Err(PlatformError::Locked);
            }
            if self
                .repeat_fence
                .is_some_and(|c| self.cancel.load(Ordering::Acquire) != c)
            {
                return Err(PlatformError::Locked);
            }
        }
        #[cfg(test)]
        if !release
            && self
                .tainted
                .as_ref()
                .is_some_and(|t| t.load(Ordering::Acquire))
        {
            return Err(PlatformError::SecureInput);
        }
        let input = encode(packet, self.tag);
        #[cfg(test)]
        {
            if let Some(owned) = self.owned_fixture
                && matches!(packet, Packet::Wheel { .. })
            {
                // SAFETY: owned fixture admission only; compare handles, never read content.
                unsafe {
                    let mut point = windows_sys::Win32::Foundation::POINT::default();
                    if GetCursorPos(&mut point) == 0
                        || WindowFromPoint(point) as usize != owned
                        || GetForegroundWindow() as usize != owned
                    {
                        return Err(PlatformError::SecureInput);
                    }
                }
            }
            self.submitted += 1;
            if let Some(sender) = &mut self.sender {
                return if sender(input) == 1 {
                    Ok(())
                } else {
                    Err(unavailable())
                };
            }
        }
        // SAFETY: one fully initialized INPUT, correct byte size; physical scancodes/mouse data
        // only. Releases deliberately bypass gates. A zero result retains owed model state.
        if unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) } == 1 {
            Ok(())
        } else {
            Err(unavailable())
        }
    }
    fn locks(&mut self) -> Result<LockKeys, PlatformError> {
        // `[U]` The calling thread has no proven foreground keyboard-queue relationship.
        // GetKeyState is queue-relative; do not turn a potentially stale bit into a toggle.
        // <https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getkeystate>
        Ok(LockKeys::default())
    }
}

fn encode(packet: Packet, tag: usize) -> INPUT {
    match packet {
        Packet::Key {
            scan,
            extended,
            down,
        } => INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: scan,
                    dwFlags: KEYEVENTF_SCANCODE
                        | if extended { KEYEVENTF_EXTENDEDKEY } else { 0 }
                        | if down { 0 } else { KEYEVENTF_KEYUP },
                    time: 0,
                    dwExtraInfo: tag,
                },
            },
        },
        _ => {
            let (dx, dy, data, flags) = match packet {
                Packet::Move { x, y } => (
                    x,
                    y,
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                ),
                Packet::Wheel { horizontal, v120 } => (
                    0,
                    0,
                    v120 as u32,
                    if horizontal {
                        MOUSEEVENTF_HWHEEL
                    } else {
                        MOUSEEVENTF_WHEEL
                    },
                ),
                Packet::Button { button, down } => {
                    let (data, press, release) = match button {
                        MouseButton::PRIMARY => (0, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
                        MouseButton::SECONDARY => (0, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
                        MouseButton::TERTIARY => (0, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
                        MouseButton::BACK => (1, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP),
                        _ => (2, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP),
                    };
                    (0, 0, data, if down { press } else { release })
                }
                Packet::Key { .. } => unreachable!(),
            };
            INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx,
                        dy,
                        mouseData: data,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: tag,
                    },
                },
            }
        }
    }
}

struct Shared {
    driver: Mutex<Driver<NativePort>>,
    cancel: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
}
struct Runtime {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    started: Instant,
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.shared.cancel.fetch_add(1, Ordering::AcqRel);
        self.shared.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Ok(mut driver) = self.shared.driver.lock() {
            driver.cancel_repeat();
            if driver.release_everything().is_err() {
                eprintln!("Windows input cleanup remains owed");
            }
        }
    }
}
thread_local! { static EVENT_GENERATION: RefCell<Option<Arc<AtomicU64>>> = const { RefCell::new(None) }; }
unsafe extern "system" fn changed(
    _: HWINEVENTHOOK,
    _: u32,
    _: windows_sys::Win32::Foundation::HWND,
    _: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    EVENT_GENERATION.with(|g| {
        if let Some(g) = g.borrow().as_ref() {
            g.fetch_add(1, Ordering::AcqRel);
        }
    });
}

fn timer(
    shared: Weak<Shared>,
    generation: Arc<AtomicU64>,
    started: Instant,
    ready: mpsc::SyncSender<bool>,
) {
    EVENT_GENERATION.with(|g| *g.borrow_mut() = Some(generation));
    // SAFETY: out-of-context callbacks run on this owned message-pump thread, with TLS state.
    let hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            null_mut(),
            Some(changed),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    let _ = ready.send(!hook.is_null());
    if hook.is_null() {
        return;
    }
    while let Some(shared) = shared.upgrade() {
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        // SAFETY: this thread's message queue only; no message contents are logged.
        unsafe {
            let mut msg = MSG::default();
            for _ in 0..64 {
                if PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) == 0 {
                    break;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if let Ok(mut driver) = shared.driver.lock() {
            let cancel = shared.cancel.load(Ordering::Acquire);
            if cancel != driver.port().repeat_owner {
                driver.cancel_repeat();
            }
            driver.port_mut().repeat_fence = Some(cancel);
            let _ = driver.repeat(started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
            driver.port_mut().repeat_fence = None;
        }
        drop(shared);
        thread::sleep(Duration::from_millis(2));
    }
    // SAFETY: remove only this thread's owned hook before TLS drops.
    unsafe {
        UnhookWinEvent(hook);
    }
    EVENT_GENERATION.with(|g| *g.borrow_mut() = None);
}

/// `[P]` Shared source and ledger; neither handle activates a target. Keep both handles for life.
pub struct WindowsKeyInjector(Arc<Runtime>);
/// `[P]` Same source as the associated key injector; caller retains IDs across monitor updates.
pub struct WindowsPointerInjector(Arc<Runtime>);

/// Opaque local-drag settlement capability on the existing shared source. It cannot submit an
/// arbitrary button or mint a window identity. Ordinary injected releases remain unchanged.
pub struct DragSettlement(Arc<Runtime>);
impl std::fmt::Debug for DragSettlement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DragSettlement(..)")
    }
}
impl WindowsPointerInjector {
    pub fn drag_settlement(&self) -> DragSettlement {
        DragSettlement(self.0.clone())
    }
}
impl DragSettlement {
    pub(crate) fn same_gate(&self, gate: &Arc<IoGate>) -> bool {
        self.0
            .shared
            .driver
            .try_lock()
            .is_ok_and(|d| Arc::ptr_eq(d.gate(), gate))
    }

    /// Source lookup is already unlocked. LL callbacks never acquire this internal Driver lock;
    /// it spans exactly one SendInput, never a capture-owner reply wait. Busy means no dispatch.
    pub(crate) fn settle(
        &self,
        target: crate::window::NativeWindow,
        nonce: usize,
        reserve: impl FnOnce() -> Result<(), PlatformError>,
        final_check: impl FnOnce() -> Result<(), PlatformError>,
    ) -> Result<LocalSettlementOutcome, PlatformError> {
        self.0.shared.cancel.fetch_add(1, Ordering::AcqRel);
        let mut driver = self.0.shared.driver.try_lock().map_err(|_| unavailable())?;
        if nonce == 0 || nonce == driver.port().tag {
            return Err(unavailable());
        }
        let expected = driver.port_mut().observe()?;
        if [
            expected.window,
            u64::from(expected.process),
            u64::from(expected.thread),
            expected.born,
        ] != [
            target.hwnd,
            u64::from(target.pid),
            u64::from(target.tid),
            target.process_created,
        ] {
            return Err(PlatformError::SecureInput);
        }
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            driver.settle_local_button(expected, reserve, |port| {
                let epoch = port.expected_gate_epoch;
                // A refused check proves this ONE local up was not submitted. No generic Err
                // after entering SendInput is ever translated into a resend obligation.
                let refused = if !port.gate.is_open() || port.gate.epoch() != epoch {
                    Some(PlatformError::Locked)
                } else if !port.observe().is_ok_and(|f| f == expected) {
                    Some(PlatformError::SecureInput)
                } else if !port.gate.is_open() || port.gate.epoch() != epoch {
                    Some(PlatformError::Locked)
                } else {
                    None
                };
                if let Some(error) = refused.or_else(|| final_check().err()) {
                    return LocalSettlementOutcome {
                        result: DragSettlementResult::KnownZero,
                        error: Some(error),
                    };
                }
                let input = encode(
                    Packet::Button {
                        button: MouseButton::PRIMARY,
                        down: false,
                    },
                    nonce,
                );
                #[cfg(test)]
                let count = if let Some(sender) = &mut port.sender {
                    sender(input)
                } else {
                    // SAFETY: one fully initialized local-primary UP; fresh target/gate checks
                    // and capture-owned protected tail precede the only dispatch.
                    unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) }
                };
                #[cfg(not(test))]
                // SAFETY: one fully initialized local-primary UP, with the same checks above.
                let count = unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) };
                match count {
                    1 => LocalSettlementOutcome {
                        result: DragSettlementResult::Accepted,
                        error: None,
                    },
                    0 => LocalSettlementOutcome {
                        result: DragSettlementResult::KnownZero,
                        error: Some(unavailable()),
                    },
                    _ => LocalSettlementOutcome {
                        result: DragSettlementResult::Uncertain,
                        error: Some(unavailable()),
                    },
                }
            })
        }));
        outcome.unwrap_or_else(|_| {
            Ok(LocalSettlementOutcome {
                result: DragSettlementResult::Uncertain,
                error: Some(unavailable()),
            })
        })
    }
}

#[cfg(test)]
mod drag_tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    fn expected() -> Foreground {
        Foreground {
            window: 1,
            process: 2,
            thread: 3,
            born: 4,
            generation: 5,
            integrity: 0x2000,
        }
    }
    fn target() -> crate::window::NativeWindow {
        crate::window::NativeWindow {
            hwnd: 1,
            pid: 2,
            tid: 3,
            process_created: 4,
            generation: 99,
        }
    }
    fn capability(
        observer: impl FnMut() -> Result<Foreground, PlatformError> + Send + 'static,
        sender: impl FnMut(INPUT) -> u32 + Send + 'static,
    ) -> (DragSettlement, Arc<IoGate>) {
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        let cancel = Arc::new(AtomicU64::new(0));
        let port = NativePort {
            gate: gate.clone(),
            generation: Arc::new(AtomicU64::new(5)),
            expected: None,
            expected_gate_epoch: 0,
            cancel: cancel.clone(),
            repeat_fence: None,
            repeat_owner: 0,
            tag: 0x43504e49,
            probes: Vec::new(),
            ids: DisplayIds::default(),
            refresh: Arc::new(|| Err(PlatformError::NotFound)),
            owned_fixture: None,
            submitted: 0,
            observer: Some(Box::new(observer)),
            sender: Some(Box::new(sender)),
            tainted: None,
        };
        let shared = Arc::new(Shared {
            driver: Mutex::new(Driver::new(
                port,
                gate.clone(),
                0x2000,
                RepeatSettings::new(0, 0).unwrap(),
            )),
            cancel,
            stop: Arc::new(AtomicBool::new(false)),
        });
        (
            DragSettlement(Arc::new(Runtime {
                shared,
                thread: None,
                started: Instant::now(),
            })),
            gate,
        )
    }

    #[test]
    fn drag_native_final_identity_and_integrity_fences_refuse_without_dispatch() {
        for changed_at in 1..=4 {
            for field in 0..6 {
                let mut calls = 0;
                let sends = Arc::new(AtomicU32::new(0));
                let count = sends.clone();
                let reserved = std::cell::Cell::new(false);
                let (cap, _) = capability(
                    move || {
                        calls += 1;
                        let mut f = expected();
                        if calls == changed_at {
                            match field {
                                0 => f.window += 1,
                                1 => f.process += 1,
                                2 => f.thread += 1,
                                3 => f.born += 1,
                                4 => f.generation += 1,
                                _ => f.integrity = 0x3000,
                            }
                        }
                        Ok(f)
                    },
                    move |_| {
                        count.fetch_add(1, Ordering::SeqCst);
                        1
                    },
                );
                let result = cap.settle(
                    target(),
                    1001,
                    || {
                        reserved.set(true);
                        Ok(())
                    },
                    || Ok(()),
                );
                assert!(
                    result.is_err()
                        || result
                            .as_ref()
                            .is_ok_and(|r| r.result == DragSettlementResult::KnownZero
                                && r.error.is_some())
                );
                assert_eq!(sends.load(Ordering::SeqCst), 0);
                assert_eq!(reserved.get(), changed_at >= 3);
            }
        }
    }

    #[test]
    fn drag_native_nonce_survives_shared_source_and_exact_count_is_only_a_report() {
        for count in [0, 1, 2] {
            let seen = Arc::new(AtomicU32::new(0));
            let sent = seen.clone();
            let (cap, _) = capability(
                || Ok(expected()),
                move |input| {
                    // SAFETY: encode selected and initialized the mouse union member.
                    unsafe {
                        assert_eq!(input.Anonymous.mi.dwExtraInfo, 1001);
                        assert_eq!(input.Anonymous.mi.dwFlags, MOUSEEVENTF_LEFTUP);
                    }
                    sent.fetch_add(1, Ordering::SeqCst);
                    count
                },
            );
            let outcome = cap.settle(target(), 1001, || Ok(()), || Ok(())).unwrap();
            assert_eq!(
                outcome.result,
                match count {
                    0 => DragSettlementResult::KnownZero,
                    1 => DragSettlementResult::Accepted,
                    _ => DragSettlementResult::Uncertain,
                }
            );
            assert_eq!(seen.load(Ordering::SeqCst), 1);
            assert!(
                lock(&cap.0.shared.driver)
                    .unwrap()
                    .held_buttons()
                    .is_empty()
            );
        }
    }

    #[test]
    fn drag_native_busy_closed_gate_and_final_activation_cancel_submit_nothing() {
        let seen = Arc::new(AtomicU32::new(0));
        let sent = seen.clone();
        let (cap, gate) = capability(
            || Ok(expected()),
            move |_| {
                sent.fetch_add(1, Ordering::SeqCst);
                1
            },
        );
        {
            let _held = lock(&cap.0.shared.driver).unwrap();
            assert!(
                cap.settle(
                    target(),
                    1001,
                    || panic!("must not reserve while busy"),
                    || Ok(())
                )
                .is_err()
            );
        }
        gate.set_engine_permits(false);
        assert!(matches!(
            cap.settle(
                target(),
                1001,
                || panic!("must not reserve while locked"),
                || Ok(())
            ),
            Err(PlatformError::Locked)
        ));
        gate.set_engine_permits(true);
        let outcome = cap
            .settle(target(), 1001, || Ok(()), || Err(PlatformError::Locked))
            .unwrap();
        assert_eq!(outcome.result, DragSettlementResult::KnownZero);
        assert_eq!(seen.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn drag_native_sender_panic_is_uncertain_and_never_adds_blind_up_to_injected_ledger() {
        let (cap, _) = capability(|| Ok(expected()), |_| panic!("fake native uncertainty"));
        let outcome = cap.settle(target(), 1001, || Ok(()), || Ok(())).unwrap();
        assert_eq!(outcome.result, DragSettlementResult::Uncertain);
        assert!(outcome.error.is_some());
        let mut driver = lock(&cap.0.shared.driver).unwrap();
        assert!(driver.held_buttons().is_empty());
        driver.release_buttons().unwrap();
    }
}

/// The caller acquires fresh native probes and its retained allocator snapshot coherently for
/// the same observation. `[P]` This is a trusted read-only acquisition seam, never a default ID
/// policy. Failures/unmatched IDs refuse the move; native rectangles are checked again below.
pub type MonitorRefresh =
    Arc<dyn Fn() -> Result<(Vec<MonitorProbe>, DisplayIds), PlatformError> + Send + Sync>;
impl std::fmt::Debug for WindowsKeyInjector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WindowsKeyInjector(..)")
    }
}
impl std::fmt::Debug for WindowsPointerInjector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WindowsPointerInjector(..)")
    }
}

/// `[P]` Creates an owned observer/repeat thread. Supplied geometry is revalidated before moves.
/// `[U]` Monitor acquisition belongs to W2.1; this adapter never guesses another DisplayId.
pub fn injectors(
    gate: Arc<IoGate>,
    probes: &[MonitorProbe],
    ids: &mut DisplayIds,
    refresh: MonitorRefresh,
) -> Result<(WindowsKeyInjector, WindowsPointerInjector), PlatformError> {
    if probes.is_empty() || probes.len() > 32 {
        return Err(PlatformError::NotFound);
    }
    displays(probes, ids).map_err(|_| PlatformError::NotFound)?;
    let mut delay = 0u32;
    let mut speed = 0u32;
    // SAFETY: read-only SPI queries write initialized u32 storage, no settings change.
    let settings = unsafe {
        if SystemParametersInfoW(SPI_GETKEYBOARDDELAY, 0, (&mut delay as *mut u32).cast(), 0) == 0
            || SystemParametersInfoW(SPI_GETKEYBOARDSPEED, 0, (&mut speed as *mut u32).cast(), 0)
                == 0
        {
            return Err(unavailable());
        }
        RepeatSettings::new(delay, speed)?
    };
    let mut lines = 0u32;
    let mut chars = 0u32;
    // SAFETY: read-only SPI queries write initialized u32 storage; no settings change.
    let scroll_settings = unsafe {
        let lines = (SystemParametersInfoW(
            SPI_GETWHEELSCROLLLINES,
            0,
            (&mut lines as *mut u32).cast(),
            0,
        ) != 0)
            .then_some(lines);
        let chars = (SystemParametersInfoW(
            SPI_GETWHEELSCROLLCHARS,
            0,
            (&mut chars as *mut u32).cast(),
            0,
        ) != 0)
            .then_some(chars);
        ScrollSettings::new(chars, lines)
    };
    // SAFETY: read-only query on our process token.
    let own = unsafe { integrity(GetCurrentProcess())? };
    let generation = Arc::new(AtomicU64::new(0));
    let cancel = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let port = NativePort {
        gate: gate.clone(),
        generation: generation.clone(),
        expected: None,
        expected_gate_epoch: 0,
        cancel: cancel.clone(),
        repeat_fence: None,
        repeat_owner: 0,
        tag: 0x43504e49,
        probes: probes.to_vec(),
        ids: ids.clone(),
        refresh,
        #[cfg(test)]
        owned_fixture: None,
        #[cfg(test)]
        submitted: 0,
        #[cfg(test)]
        observer: None,
        #[cfg(test)]
        sender: None,
        #[cfg(test)]
        tainted: None,
    };
    let mut driver = Driver::new(port, gate, own, settings);
    driver.set_scroll_settings(scroll_settings);
    let shared = Arc::new(Shared {
        driver: Mutex::new(driver),
        cancel,
        stop,
    });
    let started = Instant::now();
    let (tx, rx) = mpsc::sync_channel(1);
    let weak = Arc::downgrade(&shared);
    let handle = thread::Builder::new()
        .name("crosspane-input-repeat".into())
        .spawn(move || timer(weak, generation, started, tx))
        .map_err(|_| unavailable())?;
    let runtime = Arc::new(Runtime {
        shared,
        thread: Some(handle),
        started,
    });
    if rx.recv_timeout(Duration::from_secs(1)) != Ok(true) {
        return Err(unavailable());
    }
    Ok((
        WindowsKeyInjector(runtime.clone()),
        WindowsPointerInjector(runtime),
    ))
}

impl Drop for WindowsKeyInjector {
    fn drop(&mut self) {
        self.0.shared.cancel.fetch_add(1, Ordering::AcqRel);
        if let Ok(mut d) = self.0.shared.driver.lock() {
            d.cancel_repeat();
            if d.release_keys().is_err() {
                eprintln!("Windows key cleanup remains owed");
            }
        }
    }
}
impl Drop for WindowsPointerInjector {
    fn drop(&mut self) {
        self.0.shared.cancel.fetch_add(1, Ordering::AcqRel);
        if let Ok(mut d) = self.0.shared.driver.lock() {
            d.cancel_repeat();
            if d.release_buttons().is_err() {
                eprintln!("Windows button cleanup remains owed");
            }
        }
    }
}
impl KeyInjector for WindowsKeyInjector {
    fn key(&mut self, usage: HidUsage, down: bool) -> Result<(), PlatformError> {
        if !down {
            self.0.shared.cancel.fetch_add(1, Ordering::AcqRel);
        }
        let mut driver = lock(&self.0.shared.driver)?;
        let result = driver.key(
            usage,
            down,
            self.0
                .started
                .elapsed()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
        );
        if down {
            driver.port_mut().repeat_owner = self.0.shared.cancel.load(Ordering::Acquire);
        }
        result
    }
    fn lock_keys(&self) -> Result<LockKeys, PlatformError> {
        lock(&self.0.shared.driver)?.locks()
    }
    fn set_lock_keys(&mut self, wanted: LockKeys) -> Result<(), PlatformError> {
        self.0.shared.cancel.fetch_add(1, Ordering::AcqRel);
        lock(&self.0.shared.driver)?.set_locks(wanted)
    }
    fn release_all(&mut self) -> Result<(), PlatformError> {
        self.0.shared.cancel.fetch_add(1, Ordering::AcqRel);
        lock(&self.0.shared.driver)?.release_keys()
    }
    fn recover_keys(&mut self, keys: &[HidUsage]) -> Result<(), PlatformError> {
        self.0.shared.cancel.fetch_add(1, Ordering::AcqRel);
        lock(&self.0.shared.driver)?.recover_keys(keys)
    }
}
impl PointerInjector for WindowsPointerInjector {
    fn move_to(&mut self, display: DisplayId, position: PointDevice) -> Result<(), PlatformError> {
        let mut driver = lock(&self.0.shared.driver)?;
        let port = driver.port_mut();
        let (probes, ids) = (port.refresh)()?;
        port.probes = probes;
        port.ids = ids;
        let (packet, _) = absolute_move(&port.probes, &mut port.ids, display, position)?;
        validate_monitors(&port.probes)?;
        driver.submit_guarded(packet)
    }
    fn button(&mut self, button: MouseButton, down: bool) -> Result<(), PlatformError> {
        if !down {
            self.0.shared.cancel.fetch_add(1, Ordering::AcqRel);
        }
        lock(&self.0.shared.driver)?.button(button, down)
    }
    fn scroll(&mut self, delta: ScrollDelta) -> Result<(), PlatformError> {
        lock(&self.0.shared.driver)?.scroll(delta)
    }
    fn release_all(&mut self) -> Result<(), PlatformError> {
        self.0.shared.cancel.fetch_add(1, Ordering::AcqRel);
        lock(&self.0.shared.driver)?.release_buttons()
    }
    fn recover_buttons(&mut self, buttons: &[MouseButton]) -> Result<(), PlatformError> {
        self.0.shared.cancel.fetch_add(1, Ordering::AcqRel);
        lock(&self.0.shared.driver)?.recover_buttons(buttons)
    }
}

unsafe extern "system" fn monitor_callback(
    monitor: HMONITOR,
    _: HDC,
    _: *mut windows_sys::Win32::Foundation::RECT,
    value: isize,
) -> i32 {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: native callback HMONITOR and sized stack output, value borrows the caller's vector.
    unsafe {
        if GetMonitorInfoW(monitor, (&mut info as *mut MONITORINFOEXW).cast()) == 0 {
            return 0;
        }
        let rows = &mut *(value as *mut Vec<MONITORINFOEXW>);
        if rows.len() >= 32 {
            return 0;
        }
        rows.push(info);
    }
    1
}
fn validate_monitors(probes: &[MonitorProbe]) -> Result<(), PlatformError> {
    // SAFETY: PMv2 scope is restored on every exit, physical rects and metrics are read-only.
    unsafe {
        let old = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        if old.is_null() {
            return Err(PlatformError::NotFound);
        }
        let mut observed = Vec::<MONITORINFOEXW>::new();
        let result = EnumDisplayMonitors(
            null_mut(),
            null(),
            Some(monitor_callback),
            (&mut observed as *mut Vec<_>) as isize,
        );
        SetThreadDpiAwarenessContext(old);
        if result == 0 || probes.len() != observed.len() {
            return Err(PlatformError::NotFound);
        }
        for probe in probes {
            if !observed.iter().any(|i| {
                let end = i
                    .szDevice
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(i.szDevice.len());
                let r = i.monitorInfo.rcMonitor;
                String::from_utf16_lossy(&i.szDevice[..end]) == probe.name
                    && [r.left, r.top, r.right, r.bottom] == probe.rc_monitor
            }) {
                return Err(PlatformError::NotFound);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, AtomicU32};
    use windows_sys::Win32::{
        Foundation::{HWND, POINT, RECT},
        Graphics::Gdi::{ClientToScreen, MONITOR_DEFAULTTONEAREST, MonitorFromWindow},
        Security::{TOKEN_ELEVATION, TokenElevation, TokenUIAccess},
        System::{
            LibraryLoader::GetModuleHandleW,
            Threading::{GetCurrentProcessId, QueryFullProcessImageNameW, TerminateProcess},
        },
        UI::HiDpi::GetDpiForWindow,
    };

    #[test]
    fn packets_are_physical_scancodes_and_exact_mouse_flags() {
        let key = encode(
            Packet::Key {
                scan: 0x1d,
                extended: true,
                down: false,
            },
            123,
        );
        // SAFETY: encode selected the initialized keyboard union member.
        unsafe {
            assert_eq!(key.Anonymous.ki.wVk, 0);
            assert_eq!(
                key.Anonymous.ki.dwFlags,
                KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP
            );
            assert_eq!(key.Anonymous.ki.dwExtraInfo, 123);
        }
        let wheel = encode(
            Packet::Wheel {
                horizontal: true,
                v120: -15,
            },
            123,
        );
        // SAFETY: encode selected the initialized mouse union member.
        unsafe {
            assert_eq!(wheel.Anonymous.mi.mouseData as i32, -15);
            assert_eq!(wheel.Anonymous.mi.dwFlags, MOUSEEVENTF_HWHEEL);
        }
        for (button, data) in [(MouseButton::BACK, 1), (MouseButton::FORWARD, 2)] {
            let input = encode(Packet::Button { button, down: true }, 123);
            // SAFETY: encode selected the initialized mouse union member.
            unsafe {
                assert_eq!(input.Anonymous.mi.mouseData, data);
                assert_eq!(input.Anonymous.mi.dwFlags, MOUSEEVENTF_XDOWN);
            }
        }
    }

    #[test]
    fn final_native_fence_refuses_gate_aba_or_repeat_cancel_during_observation() {
        for mode in 0..3 {
            let gate = IoGate::new();
            gate.set_session_permits(true);
            gate.set_engine_permits(true);
            let cancel = Arc::new(AtomicU64::new(0));
            let expected = Foreground {
                window: 1,
                process: 2,
                thread: 3,
                born: 4,
                generation: 0,
                integrity: 0x2000,
            };
            let changed_gate = gate.clone();
            let changed_cancel = cancel.clone();
            let tainted = Arc::new(AtomicBool::new(false));
            let changed_taint = tainted.clone();
            let mut port = NativePort {
                expected_gate_epoch: gate.epoch(),
                gate,
                generation: Arc::new(AtomicU64::new(0)),
                expected: Some(expected),
                cancel,
                repeat_fence: Some(0),
                repeat_owner: 0,
                tag: 0,
                probes: Vec::new(),
                ids: DisplayIds::default(),
                refresh: Arc::new(|| Err(PlatformError::NotFound)),
                owned_fixture: None,
                submitted: 0,
                observer: Some(Box::new(move || {
                    if mode == 2 {
                        changed_taint.store(true, Ordering::Release);
                    } else if mode == 1 {
                        changed_cancel.fetch_add(1, Ordering::AcqRel);
                    } else {
                        changed_gate.set_engine_permits(false);
                        changed_gate.set_engine_permits(true);
                    }
                    Ok(expected)
                })),
                sender: Some(Box::new(|_| 1)),
                tainted: Some(tainted),
            };
            let result = port.submit(Packet::Key {
                scan: 0x1e,
                extended: false,
                down: true,
            });
            if mode == 2 {
                assert!(matches!(result, Err(PlatformError::SecureInput)));
            } else {
                assert!(matches!(result, Err(PlatformError::Locked)));
            }
            assert_eq!(port.submitted, 0);
        }
    }

    #[test]
    fn fresh_mapping_refusal_never_falls_back_to_cached_display() {
        for case in 0..3 {
            let probe = MonitorProbe {
                device_path: "cached".into(),
                name: "fake".into(),
                rc_monitor: [0, 0, 100, 100],
                rc_work: [0, 0, 100, 100],
                primary: true,
                dpi: 96,
                refresh_millihz: 60_000,
                edid: None,
                twin: false,
                quarter_turns: 0,
            };
            let mut ids = DisplayIds::default();
            let id = ids.assign(&probe.device_path).unwrap();
            let calls = Arc::new(AtomicU32::new(0));
            let called = calls.clone();
            let fresh_ids = ids.clone();
            let cached = probe.clone();
            let refresh: MonitorRefresh = Arc::new(move || {
                called.fetch_add(1, Ordering::AcqRel);
                if case == 0 {
                    return Err(PlatformError::Timeout);
                }
                if case == 1 {
                    let mut changed = cached.clone();
                    changed.device_path = "replacement".into();
                    return Ok((vec![changed], fresh_ids.clone()));
                }
                Ok((vec![cached.clone(), cached.clone()], fresh_ids.clone()))
            });
            let gate = IoGate::new();
            let cancel = Arc::new(AtomicU64::new(0));
            let port = NativePort {
                expected_gate_epoch: 0,
                gate: gate.clone(),
                generation: Arc::new(AtomicU64::new(0)),
                expected: None,
                cancel: cancel.clone(),
                repeat_fence: None,
                repeat_owner: 0,
                tag: 0,
                probes: vec![probe],
                ids,
                refresh,
                owned_fixture: None,
                submitted: 0,
                observer: Some(Box::new(|| Err(PlatformError::SecureInput))),
                sender: Some(Box::new(|_| 1)),
                tainted: None,
            };
            let shared = Arc::new(Shared {
                driver: Mutex::new(Driver::new(
                    port,
                    gate,
                    0x2000,
                    RepeatSettings::new(0, 31).unwrap(),
                )),
                cancel,
                stop: Arc::new(AtomicBool::new(false)),
            });
            let mut pointer = WindowsPointerInjector(Arc::new(Runtime {
                shared,
                thread: None,
                started: Instant::now(),
            }));
            let error = pointer.move_to(id, PointDevice::new(50.0, 50.0));
            if case == 0 {
                assert!(matches!(error, Err(PlatformError::Timeout)));
            } else {
                assert!(matches!(error, Err(PlatformError::NotFound)));
            }
            assert_eq!(calls.load(Ordering::Acquire), 1);
            assert_eq!(lock(&pointer.0.shared.driver).unwrap().port().submitted, 0);
        }
    }

    #[derive(Default)]
    struct Counts {
        downs: AtomicU32,
        ups: AtomicU32,
        click_downs: AtomicU32,
        click_ups: AtomicU32,
        chars: AtomicU32,
        tainted: Arc<AtomicBool>,
        expected_chars: AtomicU32,
        untagged_keys: AtomicU32,
        untagged_chars: AtomicU32,
        untagged_buttons: AtomicU32,
        wheels: AtomicU32,
        wheel_x: AtomicI64,
        wheel_y: AtomicI64,
        altgr_downs: AtomicU32,
        altgr_ups: AtomicU32,
        control_downs: AtomicU32,
        control_ups: AtomicU32,
    }
    unsafe extern "system" fn wndproc(hwnd: HWND, message: u32, w: usize, l: isize) -> isize {
        // SAFETY: WM_NCCREATE carries our create parameters; the pinned Counts outlives the HWND.
        unsafe {
            if message == WM_NCCREATE {
                let create = &*(l as *const CREATESTRUCTW);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            }
            let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Counts;
            if !state.is_null() {
                let state = &*state;
                // TranslateMessage's WM_CHAR lost dwExtraInfo in the VM. Correlate exactly
                // one count to a prior tagged own A down; never inspect any character value.
                if message == WM_CHAR {
                    if state
                        .expected_chars
                        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                        .is_ok()
                    {
                        state.chars.fetch_add(1, Ordering::AcqRel);
                    } else {
                        state.tainted.store(true, Ordering::Release);
                        state.untagged_chars.fetch_add(1, Ordering::AcqRel);
                    }
                    return 0;
                }
                let input = matches!(
                    message,
                    WM_KEYDOWN
                        | WM_SYSKEYDOWN
                        | WM_KEYUP
                        | WM_SYSKEYUP
                        | WM_LBUTTONDOWN
                        | WM_LBUTTONUP
                        | WM_MOUSEWHEEL
                        | WM_MOUSEHWHEEL
                );
                if input && GetMessageExtraInfo() as usize != 0x43504e49 {
                    state.tainted.store(true, Ordering::Release);
                    match message {
                        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                            state.untagged_buttons.fetch_add(1, Ordering::AcqRel);
                        }
                        _ => {
                            state.untagged_keys.fetch_add(1, Ordering::AcqRel);
                        }
                    }
                    return 0;
                }
                match message {
                    WM_KEYDOWN | WM_SYSKEYDOWN => {
                        state.downs.fetch_add(1, Ordering::AcqRel);
                        let scan = (l as usize >> 16) & 0x1ff;
                        if scan == 0x138 {
                            state.altgr_downs.fetch_add(1, Ordering::AcqRel);
                        } else if scan == 0x1d {
                            state.control_downs.fetch_add(1, Ordering::AcqRel);
                        }
                        // Only source-tagged injected fixture keys reach this branch.
                        if (l as usize >> 16) & 0xff == 0x1e {
                            state.expected_chars.fetch_add(1, Ordering::AcqRel);
                        }
                        return 0;
                    }
                    WM_KEYUP | WM_SYSKEYUP => {
                        state.ups.fetch_add(1, Ordering::AcqRel);
                        let scan = (l as usize >> 16) & 0x1ff;
                        if scan == 0x138 {
                            state.altgr_ups.fetch_add(1, Ordering::AcqRel);
                        } else if scan == 0x1d {
                            state.control_ups.fetch_add(1, Ordering::AcqRel);
                        }
                        return 0;
                    }
                    WM_LBUTTONDOWN => {
                        state.click_downs.fetch_add(1, Ordering::AcqRel);
                        return 0;
                    }
                    WM_LBUTTONUP => {
                        state.click_ups.fetch_add(1, Ordering::AcqRel);
                        return 0;
                    }
                    WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                        state.wheels.fetch_add(1, Ordering::AcqRel);
                        let delta = i64::from((w >> 16) as u16 as i16);
                        let axis = if message == WM_MOUSEHWHEEL {
                            &state.wheel_x
                        } else {
                            &state.wheel_y
                        };
                        axis.fetch_add(delta, Ordering::AcqRel);
                        return 0;
                    }
                    _ => {}
                }
            }
            DefWindowProcW(hwnd, message, w, l)
        }
    }
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }
    struct Fixture {
        hwnd: HWND,
        class: Vec<u16>,
        counts: Box<Counts>,
        old_dpi: windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT,
    }
    impl Fixture {
        fn new(class_name: &str) -> Self {
            let class = wide(class_name);
            let counts = Box::<Counts>::default();
            // SAFETY: own unique class, WNDPROC and pinned counts; creates only this fixture.
            unsafe {
                let old_dpi =
                    SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                assert!(!old_dpi.is_null());
                let instance = GetModuleHandleW(null());
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(wndproc),
                    hInstance: instance,
                    lpszClassName: class.as_ptr(),
                    ..Default::default()
                };
                assert_ne!(RegisterClassW(&wc), 0);
                let title = wide("Crosspane owned input fixture");
                let hwnd = CreateWindowExW(
                    WS_EX_TOOLWINDOW,
                    class.as_ptr(),
                    title.as_ptr(),
                    WS_OVERLAPPEDWINDOW,
                    150,
                    150,
                    400,
                    260,
                    null_mut(),
                    null_mut(),
                    instance,
                    (&*counts as *const Counts).cast(),
                );
                assert!(!hwnd.is_null());
                ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                Self {
                    hwnd,
                    class,
                    counts,
                    old_dpi,
                }
            }
        }
        fn pump(&self, duration: Duration) {
            let end = Instant::now() + duration;
            while Instant::now() < end {
                // SAFETY: reads this fixture's thread queue, never other windows' content.
                unsafe {
                    let mut message = MSG::default();
                    while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                        TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
                assert!(
                    !self.counts.tainted.load(Ordering::Acquire),
                    "owned fixture trial tainted; no further input"
                );
                thread::sleep(Duration::from_millis(2));
            }
        }
        fn activate(&self) {
            // SAFETY: conditional documented activation of our own fixture only, no bypass.
            unsafe {
                SetForegroundWindow(self.hwnd);
            }
            self.pump(Duration::from_millis(50));
            // SAFETY: read-only owned HWND proof.
            let foreground = unsafe { GetForegroundWindow() };
            assert_eq!(foreground, self.hwnd, "owned fixture activation refused");
        }
        fn monitor(&self) -> MonitorProbe {
            // SAFETY: read-only information for the monitor containing our own HWND.
            unsafe {
                let mut info = MONITORINFOEXW::default();
                info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
                assert_ne!(
                    GetMonitorInfoW(
                        MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST),
                        (&mut info as *mut MONITORINFOEXW).cast()
                    ),
                    0
                );
                let mut rows = Vec::<MONITORINFOEXW>::new();
                assert_ne!(
                    EnumDisplayMonitors(
                        null_mut(),
                        null(),
                        Some(monitor_callback),
                        (&mut rows as *mut Vec<_>) as isize
                    ),
                    0
                );
                assert_eq!(rows.len(), 1, "probe requires one VM monitor");
                let r = info.monitorInfo.rcMonitor;
                let w = info.monitorInfo.rcWork;
                let end = info.szDevice.iter().position(|&c| c == 0).unwrap();
                MonitorProbe {
                    device_path: "owned-probe-monitor".into(),
                    name: String::from_utf16_lossy(&info.szDevice[..end]),
                    rc_monitor: [r.left, r.top, r.right, r.bottom],
                    rc_work: [w.left, w.top, w.right, w.bottom],
                    primary: true,
                    dpi: GetDpiForWindow(self.hwnd),
                    refresh_millihz: 60_000,
                    edid: None,
                    twin: false,
                    quarter_turns: 0,
                }
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // SAFETY: destroy/unregister only this fixture, keeping Counts alive through destroy.
            unsafe {
                DestroyWindow(self.hwnd);
                UnregisterClassW(self.class.as_ptr(), GetModuleHandleW(null()));
                SetThreadDpiAwarenessContext(self.old_dpi);
            }
        }
    }
    fn limited() -> bool {
        // SAFETY: read-only current-token queries, always close owned token handle.
        unsafe {
            let mut token = null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let token = Handle(token);
            let mut elevation = TOKEN_ELEVATION::default();
            let mut access = 1u32;
            let mut size = 0;
            GetTokenInformation(
                token.0,
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut size,
            ) != 0
                && elevation.TokenIsElevated == 0
                && GetTokenInformation(
                    token.0,
                    TokenUIAccess,
                    (&mut access as *mut u32).cast(),
                    4,
                    &mut size,
                ) != 0
                && access == 0
        }
    }
    fn nonce() -> String {
        let value =
            std::env::var("CROSSPANE_W12_NONCE").expect("explicit owned probe nonce required");
        assert!(value.len() == 32 && value.bytes().all(|c| c.is_ascii_hexdigit()));
        value
    }
    struct Watchdog {
        done: Arc<AtomicBool>,
        threads: Vec<JoinHandle<()>>,
    }
    impl Watchdog {
        fn start(owned: HWND, passive: bool) -> Self {
            let done = Arc::new(AtomicBool::new(false));
            let mut threads = Vec::new();
            for seconds in [6u64, 8] {
                let done = done.clone();
                let hwnd = owned as usize;
                threads.push(thread::spawn(move || {
                    let until = Instant::now() + Duration::from_secs(seconds);
                    while !done.load(Ordering::Acquire) && Instant::now() < until {
                        thread::sleep(Duration::from_millis(10));
                    }
                    if done.load(Ordering::Acquire) {
                        return;
                    }
                    // SAFETY: bounded watchdog acts only on own fixture/current process. The
                    // separate 8s thread is independent of a stalled best-effort cleanup call.
                    unsafe {
                        if seconds == 6 && !passive && GetForegroundWindow() as usize == hwnd {
                            for usage in [0xe1, 4, 0x73] {
                                let packet = crate::model::inject::key_packet(
                                    HidUsage::keyboard(usage),
                                    false,
                                )
                                .unwrap();
                                if GetForegroundWindow() as usize != hwnd {
                                    break;
                                }
                                SendInput(
                                    1,
                                    &encode(packet, 0x43504e49),
                                    size_of::<INPUT>() as i32,
                                );
                            }
                            if GetForegroundWindow() as usize == hwnd {
                                SendInput(
                                    1,
                                    &encode(
                                        Packet::Button {
                                            button: MouseButton::PRIMARY,
                                            down: false,
                                        },
                                        0x43504e49,
                                    ),
                                    size_of::<INPUT>() as i32,
                                );
                            }
                        }
                        if seconds == 8 {
                            TerminateProcess(GetCurrentProcess(), 124);
                        }
                    }
                }));
            }
            Self { done, threads }
        }
    }
    impl Drop for Watchdog {
        fn drop(&mut self) {
            self.done.store(true, Ordering::Release);
            for thread in self.threads.drain(..) {
                let _ = thread.join();
            }
        }
    }

    fn own_process(hwnd: HWND) {
        // SAFETY: queries only the uniquely named owned fixture, without reading any content.
        unsafe {
            let mut pid = 0;
            assert_ne!(GetWindowThreadProcessId(hwnd, &mut pid), 0);
            assert_eq!(pid, GetCurrentProcessId());
        }
    }
    fn modifiers_clear() {
        // SAFETY: read-only modifier/F24/A/button preflight, never logs real key state or text.
        unsafe {
            for vk in [
                VK_LSHIFT,
                VK_RSHIFT,
                VK_LCONTROL,
                VK_RCONTROL,
                VK_LMENU,
                VK_RMENU,
                VK_LWIN,
                VK_RWIN,
                VK_F24,
                0x41,
                VK_LBUTTON,
            ] {
                assert_eq!(
                    GetAsyncKeyState(vk as i32) as u16 & 0x8000,
                    0,
                    "probe refuses held input"
                );
            }
        }
    }

    fn owned_mapping(probe: MonitorProbe, ids: DisplayIds) -> MonitorRefresh {
        Arc::new(move || {
            // The fixture has one synthetic retained ID; each callback checks the actual VM
            // monitor rectangle/name before returning its coherent observation pair.
            validate_monitors(std::slice::from_ref(&probe))?;
            Ok((vec![probe.clone()], ids.clone()))
        })
    }

    fn fixture_locks() -> [bool; 3] {
        // SAFETY: test-only, read-only query of this thread's queue state; values are not logged.
        unsafe { [VK_CAPITAL, VK_NUMLOCK, VK_SCROLL].map(|vk| GetKeyState(i32::from(vk)) & 1 != 0) }
    }

    struct LockObserver {
        requests: Option<mpsc::Sender<mpsc::Sender<[bool; 3]>>>,
        thread: Option<JoinHandle<()>>,
    }
    impl LockObserver {
        fn start() -> Self {
            let (requests, receive) = mpsc::channel::<mpsc::Sender<[bool; 3]>>();
            let thread = thread::spawn(move || {
                // SAFETY: creates this nonforeground thread's own queue once. It never
                // attaches queues or consumes the foreground fixture's keyboard messages.
                unsafe {
                    let mut message = MSG::default();
                    PeekMessageW(&mut message, null_mut(), 0, 0, PM_NOREMOVE);
                }
                while let Ok(reply) = receive.recv() {
                    let _ = reply.send(fixture_locks());
                }
            });
            Self {
                requests: Some(requests),
                thread: Some(thread),
            }
        }
        fn read(&self) -> [bool; 3] {
            let (reply, receive) = mpsc::channel();
            self.requests.as_ref().unwrap().send(reply).unwrap();
            receive.recv_timeout(Duration::from_secs(1)).unwrap()
        }
    }
    impl Drop for LockObserver {
        fn drop(&mut self) {
            self.requests.take();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn pump_restore() {
        let until = Instant::now() + Duration::from_millis(50);
        while Instant::now() < until {
            // SAFETY: pumps only this owned fixture thread's queue; no data is logged.
            unsafe {
                let mut message = MSG::default();
                while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    struct RestoreLocks<'a> {
        fixture: &'a Fixture,
        keys: &'a mut WindowsKeyInjector,
        originals: [bool; 3],
        failures: Arc<AtomicU32>,
    }
    impl RestoreLocks<'_> {
        fn owned(&self) -> bool {
            // SAFETY: compares only our retained fixture handle; no window content is read.
            (unsafe { GetForegroundWindow() == self.fixture.hwnd })
                && !self.fixture.counts.tainted.load(Ordering::Acquire)
        }
        fn toggle(&mut self, index: usize) -> Result<(), PlatformError> {
            if !self.owned() {
                return Err(PlatformError::SecureInput);
            }
            let usage = HidUsage::keyboard([0x39, 0x53, 0x47][index]);
            let down = self.keys.key(usage, true);
            let up = if self.owned() {
                self.keys.key(usage, false)
            } else {
                Err(PlatformError::SecureInput)
            };
            down.and(up)
        }
        fn restore(&mut self) {
            if !self.owned() {
                self.failures.fetch_add(1, Ordering::AcqRel);
                return;
            }
            if self.keys.release_all().is_err() {
                self.failures.fetch_add(1, Ordering::AcqRel);
            }
            pump_restore();
            for index in 0..3 {
                if fixture_locks()[index] != self.originals[index] {
                    if self.toggle(index).is_err() {
                        self.failures.fetch_add(1, Ordering::AcqRel);
                    }
                    pump_restore();
                }
            }
            if fixture_locks() != self.originals {
                self.failures.fetch_add(1, Ordering::AcqRel);
            }
        }
    }
    impl Drop for RestoreLocks<'_> {
        fn drop(&mut self) {
            // Unconditional best effort on normal return and assertion unwinding, owned-only.
            self.restore();
        }
    }

    #[test]
    #[ignore = "Limited win-gui owned fixture only; explicit nonce and W1.10 opt-in"]
    fn owned_e1_scroll_probe() {
        assert_eq!(
            std::env::var("CROSSPANE_W110_PROBE").as_deref(),
            Ok("scroll")
        );
        assert!(limited());
        let fixture = Fixture::new(&format!("CrosspaneW110ScrollLock{}", nonce()));
        let _watchdog = Watchdog::start(fixture.hwnd, false);
        fixture.activate();
        own_process(fixture.hwnd);
        modifiers_clear();
        let probe = fixture.monitor();
        let mut ids = DisplayIds::default();
        let display = ids.assign(&probe.device_path).unwrap();
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        let refresh = owned_mapping(probe.clone(), ids.clone());
        let (keys, mut pointer) =
            injectors(gate, std::slice::from_ref(&probe), &mut ids, refresh).unwrap();
        {
            let mut driver = lock(&keys.0.shared.driver).unwrap();
            driver.port_mut().owned_fixture = Some(fixture.hwnd as usize);
            driver.port_mut().tainted = Some(fixture.counts.tainted.clone());
        }
        let point = {
            // SAFETY: transforms only our retained fixture client rectangle to screen position.
            unsafe {
                let mut r = RECT::default();
                assert_ne!(GetClientRect(fixture.hwnd, &mut r), 0);
                let mut p = POINT {
                    x: (r.right - r.left) / 2,
                    y: (r.bottom - r.top) / 2,
                };
                assert_ne!(ClientToScreen(fixture.hwnd, &mut p), 0);
                PointDevice::new(
                    f64::from(p.x - probe.rc_monitor[0]),
                    f64::from(p.y - probe.rc_monitor[1]),
                )
            }
        };
        pointer.move_to(display, point).unwrap();
        fixture.pump(Duration::from_millis(50));
        // Submission acknowledgement is not routing settlement. Observe only owned-handle
        // equality for another bounded interval, with no further input while it is unresolved.
        let until = Instant::now() + Duration::from_millis(250);
        let mut observations = 0u32;
        loop {
            observations += 1;
            // SAFETY: cursor position and handle equality only; no content or coordinates logged.
            let admitted = unsafe {
                let mut cursor = POINT::default();
                assert!(
                    GetForegroundWindow() == fixture.hwnd,
                    "owned foreground lost"
                );
                assert_ne!(GetCursorPos(&mut cursor), 0);
                WindowFromPoint(cursor) == fixture.hwnd
            };
            if admitted || Instant::now() >= until {
                break;
            }
            fixture.pump(Duration::from_millis(10));
        }
        println!("OWNED_WHEEL bounded_routing_observations={}", observations);
        // SAFETY: normal focus of this thread's own window; equality queries read no content.
        unsafe {
            SetFocus(fixture.hwnd);
            let mut cursor = POINT::default();
            assert_ne!(GetCursorPos(&mut cursor), 0);
            println!(
                "OWNED_WHEEL targeting foreground={} focus={} pointer={} packets={}",
                u32::from(GetForegroundWindow() == fixture.hwnd),
                u32::from(GetFocus() == fixture.hwnd),
                u32::from(WindowFromPoint(cursor) == fixture.hwnd),
                lock(&keys.0.shared.driver).unwrap().port().submitted
            );
            assert!(
                GetFocus() == fixture.hwnd,
                "owned fixture focus unavailable"
            );
            assert!(
                WindowFromPoint(cursor) == fixture.hwnd,
                "owned pointer target unavailable"
            );
        }
        use crosspane_types::{geom::VectorLogical, input::ScrollPhase};
        let delta = ScrollDelta {
            v120_x: -15,
            v120_y: 30,
            pixels: Some(VectorLogical::new(900.0, 900.0)),
            phase: ScrollPhase::Changed,
            stop_x: false,
            stop_y: false,
        };
        pointer.scroll(delta).unwrap();
        fixture.pump(Duration::from_millis(50));
        println!(
            "OWNED_WHEEL observation packets={} x_units={} y_units={}",
            fixture.counts.wheels.load(Ordering::Acquire),
            fixture.counts.wheel_x.load(Ordering::Acquire),
            fixture.counts.wheel_y.load(Ordering::Acquire)
        );
        assert_eq!(fixture.counts.wheel_x.load(Ordering::Acquire), -15);
        assert_eq!(fixture.counts.wheel_y.load(Ordering::Acquire), 30);
        // A known test scale isolates native delivery from the separate production SPI query.
        lock(&keys.0.shared.driver)
            .unwrap()
            .set_scroll_settings(ScrollSettings::default());
        for _ in 0..2 {
            pointer
                .scroll(ScrollDelta {
                    v120_x: 0,
                    v120_y: 0,
                    pixels: Some(VectorLogical::new(0.3, -0.3)),
                    ..delta
                })
                .unwrap();
        }
        fixture.pump(Duration::from_millis(50));
        assert_eq!(fixture.counts.wheel_x.load(Ordering::Acquire), -14);
        assert_eq!(fixture.counts.wheel_y.load(Ordering::Acquire), 29);
        let wheels = fixture.counts.wheels.load(Ordering::Acquire);
        pointer
            .scroll(ScrollDelta {
                v120_x: 0,
                v120_y: 0,
                pixels: None,
                phase: ScrollPhase::Ended,
                stop_x: true,
                stop_y: true,
            })
            .unwrap();
        fixture.pump(Duration::from_millis(50));
        assert_eq!(fixture.counts.wheels.load(Ordering::Acquire), wheels);
        println!("OWNED_WHEEL verified_packets={}", wheels);
    }

    fn observe_owned_locks(fixture: &Fixture, keys: &mut WindowsKeyInjector) {
        let observer = LockObserver::start();
        let originals = fixture_locks();
        let failures = Arc::new(AtomicU32::new(0));
        let mut restore = RestoreLocks {
            fixture,
            keys,
            originals,
            failures: failures.clone(),
        };
        let mut background_current = 0u32;
        let mut background_stale = 0u32;
        for index in 0..3 {
            let before = observer.read();
            if let Err(error) = restore.toggle(index) {
                drop(restore);
                println!(
                    "OWNED_LOCK refusal completed_trials={} submitted={} downs={} ups={} restore_failures={}",
                    background_current + background_stale,
                    lock(&keys.0.shared.driver).unwrap().port().submitted,
                    fixture.counts.downs.load(Ordering::Acquire),
                    fixture.counts.ups.load(Ordering::Acquire),
                    failures.load(Ordering::Acquire)
                );
                panic!("owned lock trial refused: {error}");
            }
            fixture.pump(Duration::from_millis(70));
            let foreground = fixture_locks();
            assert!(
                foreground[index] != originals[index],
                "owned lock did not toggle"
            );
            let after = observer.read();
            if after[index] == foreground[index] {
                background_current += 1;
            } else {
                background_stale += 1;
                assert!(
                    after[index] == before[index],
                    "background state was inconsistent"
                );
            }
            restore.restore();
            assert!(
                fixture_locks() == originals,
                "owned original lock restoration failed"
            );
        }
        drop(restore);
        assert_eq!(failures.load(Ordering::Acquire), 0);
        assert!(
            fixture_locks() == originals,
            "owned original lock restoration failed"
        );
        assert_eq!(fixture.counts.downs.load(Ordering::Acquire), 6);
        assert_eq!(fixture.counts.ups.load(Ordering::Acquire), 6);
        println!(
            "OWNED_LOCK balanced_lock_downs=6 ups=6 background_current={} background_stale={} restore_failures=0",
            background_current, background_stale
        );
    }

    #[test]
    #[ignore = "Limited win-gui owned foreground lock fixture only; explicit W1.10 opt-in"]
    fn owned_e1_lock_probe() {
        assert_eq!(
            std::env::var("CROSSPANE_W110_PROBE").as_deref(),
            Ok("locks")
        );
        assert!(limited());
        let fixture = Fixture::new(&format!("CrosspaneW110Locks{}", nonce()));
        let _watchdog = Watchdog::start(fixture.hwnd, false);
        fixture.activate();
        own_process(fixture.hwnd);
        modifiers_clear();
        let probe = fixture.monitor();
        let mut ids = DisplayIds::default();
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        let refresh = owned_mapping(probe.clone(), ids.clone());
        let (mut keys, _pointer) =
            injectors(gate, std::slice::from_ref(&probe), &mut ids, refresh).unwrap();
        {
            let mut driver = lock(&keys.0.shared.driver).unwrap();
            driver.port_mut().owned_fixture = Some(fixture.hwnd as usize);
            driver.port_mut().tainted = Some(fixture.counts.tainted.clone());
        }
        observe_owned_locks(&fixture, &mut keys);
        modifiers_clear();
    }

    #[test]
    #[ignore = "Limited win-gui metadata only; active installed layout, no layout changes"]
    fn owned_altgr_metadata_probe() {
        assert_eq!(
            std::env::var("CROSSPANE_W110_PROBE").as_deref(),
            Ok("altgr")
        );
        assert!(limited());
        let _nonce = nonce();
        // SAFETY: read-only installed/current layout queries and translation of fixture constants.
        // VkKeyScanEx does not generate input, change layouts or inspect owner typing.
        let (installed, active_applicable) = unsafe {
            let count = GetKeyboardLayoutList(0, null_mut());
            assert!((1..=128).contains(&count));
            let mut layouts = vec![null_mut(); count as usize];
            let read = GetKeyboardLayoutList(count, layouts.as_mut_ptr());
            assert_eq!(read, count);
            let applicable = |layout| {
                [0x40u16, 0x20ac, 0x7c].into_iter().any(|character| {
                    let mapping = VkKeyScanExW(character, layout);
                    mapping != -1 && ((mapping as u16 >> 8) & 6) == 6
                })
            };
            let installed = layouts
                .into_iter()
                .filter(|layout| applicable(*layout))
                .count();
            (installed, applicable(GetKeyboardLayout(0)))
        };
        if !active_applicable {
            println!(
                "OWNED_ALTGR applicable_installed={} active_applicable=0 observed=0 deferred_W6_1=1",
                installed
            );
            return;
        }
        let fixture = Fixture::new(&format!("CrosspaneW110AltGr{}", nonce()));
        let _watchdog = Watchdog::start(fixture.hwnd, false);
        fixture.activate();
        own_process(fixture.hwnd);
        modifiers_clear();
        let probe = fixture.monitor();
        let mut ids = DisplayIds::default();
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        let refresh = owned_mapping(probe.clone(), ids.clone());
        let (mut keys, _pointer) =
            injectors(gate, std::slice::from_ref(&probe), &mut ids, refresh).unwrap();
        {
            let mut driver = lock(&keys.0.shared.driver).unwrap();
            driver.port_mut().owned_fixture = Some(fixture.hwnd as usize);
            driver.port_mut().tainted = Some(fixture.counts.tainted.clone());
        }
        let down = keys.key(HidUsage::keyboard(0xe6), true);
        let up = keys.key(HidUsage::keyboard(0xe6), false);
        down.and(up).unwrap();
        fixture.pump(Duration::from_millis(100));
        keys.release_all().unwrap();
        modifiers_clear();
        let count = |counter: &AtomicU32| counter.load(Ordering::Acquire);
        assert_eq!(
            count(&fixture.counts.altgr_downs),
            count(&fixture.counts.altgr_ups)
        );
        assert_eq!(
            count(&fixture.counts.control_downs),
            count(&fixture.counts.control_ups)
        );
        println!(
            "OWNED_ALTGR applicable_installed={} active_applicable=1 altgr_downs={} altgr_ups={} ctrl_downs={} ctrl_ups={} hardware_classification_unproven=1",
            installed,
            count(&fixture.counts.altgr_downs),
            count(&fixture.counts.altgr_ups),
            count(&fixture.counts.control_downs),
            count(&fixture.counts.control_ups)
        );
    }

    #[test]
    #[ignore = "Limited win-gui owned fixture only; explicit nonce and opt-in"]
    fn owned_medium_key_click_probe() {
        assert_eq!(
            std::env::var("CROSSPANE_W12_PROBE").as_deref(),
            Ok("medium")
        );
        assert!(limited());
        let fixture = Fixture::new(&format!("CrosspaneW12Medium{}", nonce()));
        let _watchdog = Watchdog::start(fixture.hwnd, false);
        fixture.activate();
        own_process(fixture.hwnd);
        modifiers_clear();
        let probe = fixture.monitor();
        let mut ids = DisplayIds::default();
        let display = ids.assign(&probe.device_path).unwrap();
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        let refresh = owned_mapping(probe.clone(), ids.clone());
        let (mut keys, mut pointer) =
            injectors(gate, std::slice::from_ref(&probe), &mut ids, refresh).unwrap();
        lock(&keys.0.shared.driver)
            .unwrap()
            .port_mut()
            .owned_fixture = Some(fixture.hwnd as usize);
        lock(&keys.0.shared.driver).unwrap().port_mut().tainted =
            Some(fixture.counts.tainted.clone());
        for (id, down) in [(0xe1, true), (4, true), (4, false), (0xe1, false)] {
            own_process(fixture.hwnd);
            keys.key(HidUsage::keyboard(id), down).unwrap();
            fixture.pump(Duration::from_millis(30));
        }
        let point = {
            // SAFETY: own client rectangle transformed to physical screen under PMv2.
            unsafe {
                let mut r = RECT::default();
                assert_ne!(GetClientRect(fixture.hwnd, &mut r), 0);
                let mut p = POINT {
                    x: (r.right - r.left) / 2,
                    y: (r.bottom - r.top) / 2,
                };
                assert_ne!(ClientToScreen(fixture.hwnd, &mut p), 0);
                PointDevice::new(
                    f64::from(p.x - probe.rc_monitor[0]),
                    f64::from(p.y - probe.rc_monitor[1]),
                )
            }
        };
        pointer.move_to(display, point).unwrap();
        pointer.button(MouseButton::PRIMARY, true).unwrap();
        pointer.button(MouseButton::PRIMARY, false).unwrap();
        fixture.pump(Duration::from_millis(100));
        keys.key(HidUsage::keyboard(0x73), true).unwrap();
        fixture.pump(Duration::from_millis(1100));
        keys.key(HidUsage::keyboard(0x73), false).unwrap();
        fixture.pump(Duration::from_millis(100));
        keys.release_all().unwrap();
        pointer.release_all().unwrap();
        modifiers_clear();
        println!(
            "OWNED attribution diagnostics: untagged_keys={} untagged_chars={} untagged_buttons={}",
            fixture.counts.untagged_keys.load(Ordering::Acquire),
            fixture.counts.untagged_chars.load(Ordering::Acquire),
            fixture.counts.untagged_buttons.load(Ordering::Acquire)
        );
        assert!(
            !fixture.counts.tainted.load(Ordering::Acquire),
            "physical input tainted owned trial"
        );
        assert!(fixture.counts.downs.load(Ordering::Acquire) >= 4);
        assert_eq!(fixture.counts.ups.load(Ordering::Acquire), 3);
        assert_eq!(fixture.counts.click_downs.load(Ordering::Acquire), 1);
        assert_eq!(fixture.counts.click_ups.load(Ordering::Acquire), 1);
        modifiers_clear();
        println!(
            "OWNED_MEDIUM balanced sequence+click; repeats={}; chars={}; modifier pre/post clear",
            fixture.counts.downs.load(Ordering::Acquire) - 3,
            fixture.counts.chars.load(Ordering::Acquire)
        );
    }

    #[test]
    #[ignore = "Passive HIGH fixture only through --high-target; no input submitted"]
    fn owned_passive_high_fixture() {
        assert_eq!(std::env::var("CROSSPANE_W12_PROBE").as_deref(), Ok("high"));
        // SAFETY: read-only own token query; only this passive fixture may run elevated.
        assert!(unsafe { integrity(GetCurrentProcess()) }.unwrap() >= 0x3000);
        let fixture = Fixture::new(&format!("CrosspaneW12High{}", nonce()));
        let _watchdog = Watchdog::start(fixture.hwnd, true);
        fixture.pump(Duration::from_secs(7));
        assert_eq!(fixture.counts.downs.load(Ordering::Acquire), 0);
        assert_eq!(fixture.counts.click_downs.load(Ordering::Acquire), 0);
    }

    #[test]
    #[ignore = "Limited HIGH-refusal controller under shared win-gui lifecycle lock"]
    fn owned_high_refusal_probe() {
        assert_eq!(
            std::env::var("CROSSPANE_W12_PROBE").as_deref(),
            Ok("refusal")
        );
        assert!(limited());
        let token = nonce();
        let fixture = Fixture::new(&format!("CrosspaneW12Controller{token}"));
        let _watchdog = Watchdog::start(fixture.hwnd, false);
        fixture.activate();
        modifiers_clear();
        let class = wide(&format!("CrosspaneW12High{token}"));
        let until = Instant::now() + Duration::from_secs(2);
        let high = loop {
            // SAFETY: searches only the unique nonce class of the explicitly launched own fixture.
            let found = unsafe { FindWindowW(class.as_ptr(), null()) };
            if !found.is_null() {
                break found;
            }
            assert!(Instant::now() < until, "owned high fixture absent");
            fixture.pump(Duration::from_millis(10));
        };
        // SAFETY: read-only exact-owned fixture PID/executable/token correlation; no titles/content.
        unsafe {
            let mut pid = 0;
            assert_ne!(GetWindowThreadProcessId(high, &mut pid), 0);
            assert_ne!(pid, GetCurrentProcessId());
            let process = Handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid));
            assert!(!process.0.is_null());
            let mut path = vec![0u16; 32768];
            let mut size = path.len() as u32;
            assert_ne!(
                QueryFullProcessImageNameW(process.0, 0, path.as_mut_ptr(), &mut size),
                0
            );
            assert_eq!(
                String::from_utf16(&path[..size as usize])
                    .unwrap()
                    .to_lowercase(),
                std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .to_lowercase()
            );
            assert!(integrity(process.0).unwrap() >= 0x3000);
            SetForegroundWindow(high);
        }
        fixture.pump(Duration::from_millis(50));
        // SAFETY: exact own HIGH HWND identity check; failure refuses the trial.
        let foreground = unsafe { GetForegroundWindow() };
        assert_eq!(foreground, high, "owned high activation refused");
        let probe = fixture.monitor();
        let mut ids = DisplayIds::default();
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        ids.assign(&probe.device_path).unwrap();
        let refresh = owned_mapping(probe.clone(), ids.clone());
        let (mut keys, mut pointer) = injectors(gate, &[probe], &mut ids, refresh).unwrap();
        lock(&keys.0.shared.driver)
            .unwrap()
            .port_mut()
            .owned_fixture = Some(high as usize);
        assert!(matches!(
            keys.key(HidUsage::keyboard(4), true),
            Err(PlatformError::SecureInput)
        ));
        assert!(matches!(
            pointer.button(MouseButton::PRIMARY, true),
            Err(PlatformError::SecureInput)
        ));
        assert_eq!(lock(&keys.0.shared.driver).unwrap().port().submitted, 0);
        fixture.activate();
        modifiers_clear();
        println!(
            "OWNED_HIGH verified executable+integrity; SecureInput; zero SendInput; modifier pre/post clear"
        );
    }
}
