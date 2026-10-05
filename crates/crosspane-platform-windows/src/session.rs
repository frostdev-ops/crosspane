//! Windows session observation. The owned top-level window is never shown or activated.
//! The observer closes the session gate before emitting safety events.

#![allow(unsafe_code)]

use std::cell::{Cell, RefCell};
use std::fmt;
use std::mem::{size_of, size_of_val};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{null, null_mut};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use crosspane_platform::{
    EventSink, IoGate, PlatformError, SessionEvent, SessionEvents, SessionState,
};
use windows_sys::Win32::Foundation::{
    GetLastError, HWND, LPARAM, LRESULT, WAIT_FAILED, WAIT_TIMEOUT, WPARAM,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Power::{
    HPOWERNOTIFY, RegisterSuspendResumeNotification, UnregisterSuspendResumeNotification,
};
use windows_sys::Win32::System::RemoteDesktop::*;
use windows_sys::Win32::System::StationsAndDesktops::*;
use windows_sys::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows_sys::Win32::System::WindowsProgramming::QueryUnbiasedInterruptTime;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::model::session::{Reading, SessionModel, Signal, UNKNOWN, missed_sleep};

const POLL: Duration = Duration::from_millis(500);
const OBSERVATION_BOUND: Duration = Duration::from_secs(2);

struct Data {
    model: SessionModel,
    sink: Option<Arc<dyn EventSink<SessionEvent>>>,
}

struct Shared {
    gate: Arc<IoGate>,
    data: Mutex<Data>,
    emission: Mutex<()>,
    stopped: AtomicBool,
    refresh: AtomicBool,
    progress: Mutex<Instant>,
    changed: Condvar,
}

impl Shared {
    fn new(gate: Arc<IoGate>) -> Self {
        gate.set_session_permits(false);
        Self {
            gate,
            data: Mutex::new(Data {
                model: SessionModel::default(),
                sink: None,
            }),
            emission: Mutex::new(()),
            stopped: AtomicBool::new(false),
            refresh: AtomicBool::new(true),
            progress: Mutex::new(Instant::now()),
            changed: Condvar::new(),
        }
    }

    fn data(&self) -> MutexGuard<'_, Data> {
        match self.data.lock() {
            Ok(data) => data,
            Err(poison) => {
                self.gate.set_session_permits(false);
                self.stopped.store(true, Ordering::Release);
                let mut data = poison.into_inner();
                data.model.signal(Signal::Lost);
                data
            }
        }
    }

    fn emission(&self) -> MutexGuard<'_, ()> {
        match self.emission.lock() {
            Ok(guard) => guard,
            Err(poison) => {
                self.gate.set_session_permits(false);
                self.stopped.store(true, Ordering::Release);
                self.data().model.signal(Signal::Lost);
                poison.into_inner()
            }
        }
    }

    fn state(&self) -> SessionState {
        if self.stopped.load(Ordering::Acquire) {
            UNKNOWN
        } else {
            self.data().model.state()
        }
    }

    fn begin_read(&self) -> u64 {
        self.data().model.begin_read()
    }

    // The emission lock preserves subscription order. No data lock is held over send;
    // a nonblocking sink may inspect state. A broken sink cannot unwind through Win32.
    fn send(&self, sink: Option<Arc<dyn EventSink<SessionEvent>>>, events: Vec<SessionEvent>) {
        if let Some(sink) = sink {
            for event in events {
                if catch_unwind(AssertUnwindSafe(|| sink.send(event))).is_err() {
                    self.gate.set_session_permits(false);
                    self.stopped.store(true, Ordering::Release);
                    let mut data = self.data();
                    data.model.signal(Signal::Lost);
                    data.sink = None;
                    drop(data);
                    let _ =
                        catch_unwind(AssertUnwindSafe(|| sink.send(SessionEvent::State(UNKNOWN))));
                    self.changed.notify_all();
                    return;
                }
            }
        }
    }

    fn complete(&self, revision: u64, reading: Reading) {
        let _emission = self.emission();
        if self.stopped.load(Ordering::Acquire) {
            return;
        }
        let mut data = self.data();
        let events = data.model.complete(revision, reading);
        self.gate.set_session_permits(
            !self.stopped.load(Ordering::Acquire) && data.model.state().permits_io(),
        );
        // Loss may be announced while this delivery holds the emission lock. Its atomic
        // stop is set before gate closure, so a racing completed read cannot undo it.
        if self.stopped.load(Ordering::Acquire) {
            self.gate.set_session_permits(false);
            return;
        }
        let sink = data.sink.clone();
        drop(data);
        self.send(sink, events);
    }

    fn signal(&self, signal: Signal) {
        if matches!(signal, Signal::Lost) {
            self.stopped.store(true, Ordering::Release);
        }
        // Safety closure precedes waiting for serialized event delivery.
        self.gate.set_session_permits(false);
        let _emission = self.emission();
        if self.stopped.load(Ordering::Acquire) && !matches!(signal, Signal::Lost) {
            return;
        }
        let mut data = self.data();
        let mut events = data.model.signal(signal);
        if events.is_empty() && matches!(signal, Signal::Lost) {
            events.push(SessionEvent::State(UNKNOWN));
        }
        let sink = data.sink.clone();
        drop(data);
        self.send(sink, events);
        self.refresh.store(true, Ordering::Release);
        self.changed.notify_all();
    }

    fn subscribe(&self, sink: Arc<dyn EventSink<SessionEvent>>) -> Result<(), PlatformError> {
        let _emission = self.emission();
        let mut data = self.data();
        if self.stopped.load(Ordering::Acquire) {
            return Err(PlatformError::Backend(
                "Windows session observer stopped".into(),
            ));
        }
        if data.sink.is_some() {
            return Err(PlatformError::Backend(
                "SessionEvents::subscribe called twice".into(),
            ));
        }
        data.sink = Some(sink.clone());
        let state = data.model.state();
        drop(data);
        self.send(Some(sink), vec![SessionEvent::State(state)]);
        if self.stopped.load(Ordering::Acquire) {
            Err(PlatformError::Backend("Windows session sink failed".into()))
        } else {
            Ok(())
        }
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.gate.set_session_permits(false);
        let _emission = self.emission();
        let mut data = self.data();
        data.model.signal(Signal::Lost);
        data.sink = None;
        self.changed.notify_all();
    }

    fn touched(&self) {
        match self.progress.lock() {
            Ok(mut progress) => *progress = Instant::now(),
            Err(_) => self.signal(Signal::Lost),
        }
        self.changed.notify_all();
    }

    fn watchdog(&self) {
        while !self.stopped.load(Ordering::Acquire) {
            let progress = match self.progress.lock() {
                Ok(progress) => progress,
                Err(_) => {
                    self.signal(Signal::Lost);
                    return;
                }
            };
            if progress.elapsed() > OBSERVATION_BOUND {
                drop(progress);
                self.signal(Signal::Lost);
                return;
            }
            match self.changed.wait_timeout(progress, POLL) {
                Ok(_) => {}
                Err(_) => {
                    self.signal(Signal::Lost);
                    return;
                }
            }
        }
    }
}

struct ThreadExit(Arc<Shared>);

impl Drop for ThreadExit {
    fn drop(&mut self) {
        self.0.signal(Signal::Lost);
    }
}

/// Observes only this process's session. Construction starts observation before subscription.
/// The never-shown window, registrations and class belong exclusively to its worker thread.
pub struct WindowsSession {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    watchdog: Option<JoinHandle<()>>,
}

impl fmt::Debug for WindowsSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowsSession")
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl WindowsSession {
    /// Starts a read-only session observer and immediately closes its side of `gate`.
    pub fn new(gate: Arc<IoGate>) -> Result<Self, PlatformError> {
        let shared = Arc::new(Shared::new(gate));
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let observed = shared.clone();
        let worker = thread::Builder::new()
            .name("crosspane-win-session".into())
            .spawn(move || {
                let _exit = ThreadExit(observed.clone());
                match Native::new(observed.clone()) {
                    Ok(native) => {
                        let mut clock = None;
                        native.poll(&mut clock);
                        if ready_tx.send(Ok(())).is_ok() {
                            let _ = native.run(clock);
                        }
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                    }
                }
            })
            .map_err(|e| PlatformError::Backend(format!("spawn Windows session observer: {e}")))?;
        let mut session = Self {
            shared,
            worker: Some(worker),
            watchdog: None,
        };
        let watched = session.shared.clone();
        session.watchdog = Some(
            thread::Builder::new()
                .name("crosspane-session-watch".into())
                .spawn(move || {
                    let _exit = ThreadExit(watched.clone());
                    watched.watchdog();
                })
                .map_err(|e| {
                    PlatformError::Backend(format!("spawn Windows session watchdog: {e}"))
                })?,
        );
        ready_rx
            .recv_timeout(OBSERVATION_BOUND)
            .map_err(|e| match e {
                mpsc::RecvTimeoutError::Timeout => PlatformError::Timeout,
                mpsc::RecvTimeoutError::Disconnected => {
                    PlatformError::Backend("Windows session observer died during startup".into())
                }
            })??;
        if session.shared.stopped.load(Ordering::Acquire) {
            return Err(PlatformError::Backend(
                "Windows session observer unavailable".into(),
            ));
        }
        Ok(session)
    }
}

impl SessionEvents for WindowsSession {
    fn state(&self) -> SessionState {
        self.shared.state()
    }

    fn subscribe(&mut self, sink: Arc<dyn EventSink<SessionEvent>>) -> Result<(), PlatformError> {
        self.shared.subscribe(sink)
    }
}

impl Drop for WindowsSession {
    fn drop(&mut self) {
        self.shared.stop();
        let deadline = Instant::now() + OBSERVATION_BOUND;
        for worker in [&mut self.worker, &mut self.watchdog] {
            if let Some(worker) = worker.take() {
                while !worker.is_finished() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
                if worker.is_finished() {
                    let _ = worker.join();
                }
                // An OS call cannot safely be killed. A stalled worker retains its own native
                // resources until that call returns; the stopped shared state never reopens.
            }
        }
    }
}

#[derive(Clone)]
struct Context {
    shared: Arc<Shared>,
    session: u32,
    alive: Rc<Cell<bool>>,
}

thread_local! { static CONTEXT: RefCell<Option<Context>> = const { RefCell::new(None) }; }

fn os_error(operation: &str) -> PlatformError {
    // SAFETY: reads only this thread's last-error scalar; never user data.
    PlatformError::Backend(format!("{operation}: Win32 error {}", unsafe {
        GetLastError()
    }))
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    let context = CONTEXT.with(|value| value.try_borrow().ok().and_then(|value| value.clone()));
    if let Some(context) = context {
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let signal = match message {
                WM_WTSSESSION_CHANGE => Some(if lp != context.session as isize {
                    Signal::Refresh
                } else {
                    match wp as u32 {
                        WTS_SESSION_LOCK => Signal::Lock,
                        WTS_SESSION_UNLOCK => Signal::Unlock,
                        WTS_CONSOLE_DISCONNECT | WTS_REMOTE_DISCONNECT | WTS_SESSION_LOGOFF => {
                            Signal::Inactive
                        }
                        _ => Signal::Refresh,
                    }
                }),
                WM_POWERBROADCAST => match wp as u32 {
                    PBT_APMSUSPEND => Some(Signal::Sleep),
                    PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND | PBT_APMRESUMECRITICAL => {
                        Some(Signal::Wake)
                    }
                    _ => None,
                },
                WM_NCDESTROY => {
                    context.alive.set(false);
                    Some(Signal::Lost)
                }
                _ => None,
            };
            if let Some(signal) = signal {
                context.shared.signal(signal);
            }
        }));
        if outcome.is_err() {
            context.shared.signal(Signal::Lost);
        }
        if message == WM_POWERBROADCAST {
            return 1;
        }
    }
    // SAFETY: forwards original arguments for our registered window class; no input is interpreted.
    unsafe { DefWindowProcW(window, message, wp, lp) }
}

struct Native {
    context: Context,
    class: Vec<u16>,
    instance: windows_sys::Win32::Foundation::HINSTANCE,
    registered_class: bool,
    window: HWND,
    wts: bool,
    power: HPOWERNOTIFY,
}

impl Native {
    fn new(shared: Arc<Shared>) -> Result<Self, PlatformError> {
        let mut session = 0;
        // SAFETY: only our process/thread/module; session is a valid writable scalar.
        let (instance, tid) = unsafe {
            if ProcessIdToSessionId(GetCurrentProcessId(), &mut session) == 0 {
                return Err(os_error("process session"));
            }
            (GetModuleHandleW(null()), GetCurrentThreadId())
        };
        if instance.is_null() {
            return Err(os_error("session module"));
        }
        let context = Context {
            shared,
            session,
            alive: Rc::new(Cell::new(true)),
        };
        CONTEXT.with(|value| *value.borrow_mut() = Some(context.clone()));
        let mut native = Self {
            context,
            class: format!("CrosspaneSession.{tid}")
                .encode_utf16()
                .chain([0])
                .collect(),
            instance,
            registered_class: false,
            window: null_mut(),
            wts: false,
            power: 0,
        };
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: native.class.as_ptr(),
            ..Default::default()
        };
        // SAFETY: static callback ABI, valid immutable class name, borrowed module; no visible style.
        unsafe {
            if RegisterClassW(&class) == 0 {
                return Err(os_error("register session class"));
            }
            native.registered_class = true;
            native.window = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                native.class.as_ptr(),
                native.class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            if native.window.is_null() {
                return Err(os_error("create session observer"));
            }
            if WTSRegisterSessionNotification(native.window, NOTIFY_FOR_THIS_SESSION) == 0 {
                return Err(os_error("register WTS observer"));
            }
            native.wts = true;
            native.power =
                RegisterSuspendResumeNotification(native.window, DEVICE_NOTIFY_WINDOW_HANDLE);
            if native.power == 0 {
                return Err(os_error("register suspend/resume observer"));
            }
        }
        Ok(native)
    }

    fn poll(&self, previous: &mut Option<Clock>) {
        let current = Clock::read();
        let clocks_valid = match (&*previous, &current) {
            (Some(before), Some(now)) => match (
                now.wall.duration_since(before.wall),
                now.awake.checked_sub(before.awake),
            ) {
                (Ok(wall), Some(awake)) => {
                    if missed_sleep(
                        now.monotonic.duration_since(before.monotonic),
                        wall,
                        Duration::from_nanos(awake.saturating_mul(100)),
                    ) {
                        self.context.shared.signal(Signal::MissedSleep);
                    }
                    true
                }
                _ => false,
            },
            (None, Some(_)) => true,
            _ => false,
        };
        *previous = current;
        // A wake invalidates the generation BEFORE this OS read starts.
        let revision = self.context.shared.begin_read();
        let reading = if clocks_valid {
            read_state(self.context.session)
        } else {
            Reading::UNKNOWN
        };
        self.context.shared.complete(revision, reading);
        self.context.shared.touched();
    }

    fn run(&self, mut clock: Option<Clock>) -> Result<(), PlatformError> {
        let mut last_poll = Instant::now();
        while !self.context.shared.stopped.load(Ordering::Acquire) {
            let mut message = MSG::default();
            // SAFETY: only this worker's message queue; no owner windows are queried or dispatched.
            unsafe {
                while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    if message.message == WM_QUIT {
                        return Err(PlatformError::Backend(
                            "Windows session message loop ended".into(),
                        ));
                    }
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                    if self.context.shared.stopped.load(Ordering::Acquire) {
                        break;
                    }
                }
            }
            if self.context.shared.refresh.swap(false, Ordering::AcqRel)
                || last_poll.elapsed() >= POLL
            {
                self.poll(&mut clock);
                last_poll = Instant::now();
            }
            // SAFETY: zero handles, queue wake only. 50ms bound also observes stop without posting
            // to an HWND/thread ID that might have been recycled after observer failure.
            let result = unsafe {
                MsgWaitForMultipleObjectsEx(0, null(), 50, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
            };
            if result == WAIT_FAILED {
                return Err(os_error("wait for session messages"));
            }
            if result != 0 && result != WAIT_TIMEOUT {
                return Err(PlatformError::Backend(
                    "unexpected session wait result".into(),
                ));
            }
        }
        Ok(())
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        // A failed message loop must close before any potentially stalled OS teardown.
        self.context.shared.signal(Signal::Lost);
        // SAFETY: only registrations/class/window created on this worker; NCDESTROY tracks early
        // destruction, preventing use of a recycled HWND. The window is never shown/activated.
        unsafe {
            if self.power != 0 && UnregisterSuspendResumeNotification(self.power) == 0 {
                self.context.shared.signal(Signal::Lost);
            }
            if self.wts
                && self.context.alive.get()
                && WTSUnRegisterSessionNotification(self.window) == 0
            {
                self.context.shared.signal(Signal::Lost);
            }
            if !self.window.is_null() && self.context.alive.get() && DestroyWindow(self.window) == 0
            {
                self.context.shared.signal(Signal::Lost);
            }
            if self.registered_class && UnregisterClassW(self.class.as_ptr(), self.instance) == 0 {
                self.context.shared.signal(Signal::Lost);
            }
        }
        CONTEXT.with(|value| *value.borrow_mut() = None);
    }
}

struct Clock {
    monotonic: Instant,
    wall: SystemTime,
    awake: u64,
}

impl Clock {
    fn read() -> Option<Self> {
        let mut awake = 0;
        // SAFETY: valid output scalar; unbiased time excludes system sleep/hibernation.
        if unsafe { QueryUnbiasedInterruptTime(&mut awake) } == 0 {
            return None;
        }
        Some(Self {
            monotonic: Instant::now(),
            wall: SystemTime::now(),
            awake,
        })
    }
}

fn read_state(expected_session: u32) -> Reading {
    let mut session = 0;
    // SAFETY: own process and valid output scalar; console ID is a read-only scalar.
    let console = unsafe {
        if ProcessIdToSessionId(GetCurrentProcessId(), &mut session) == 0
            || session != expected_session
        {
            return Reading::UNKNOWN;
        }
        WTSGetActiveConsoleSessionId()
    };
    if console == u32::MAX {
        return Reading::UNKNOWN;
    }
    let connected = match connection_state(session) {
        Some(value) => value,
        None => return Reading::UNKNOWN,
    };
    let default_desktop = match default_input_desktop() {
        Some(value) => value,
        None => return Reading::UNKNOWN,
    };
    // SAFETY: a second read rejects console switches during the multi-call observation.
    if unsafe { WTSGetActiveConsoleSessionId() } != console {
        return Reading::UNKNOWN;
    }
    Reading {
        console: Some(console == session),
        connected: Some(connected),
        default_desktop: Some(default_desktop),
    }
}

fn connection_state(session: u32) -> Option<bool> {
    let mut buffer = null_mut();
    let mut bytes = 0;
    // SAFETY: output pointers initialized null/zero; only this session on the local WTS server.
    let ok = unsafe {
        WTSQuerySessionInformationW(
            null_mut(),
            session,
            WTSConnectState,
            &mut buffer,
            &mut bytes,
        )
    };
    let result =
        if ok != 0 && !buffer.is_null() && bytes as usize == size_of::<WTS_CONNECTSTATE_CLASS>() {
            // SAFETY: API-owned buffer has the checked enum size; unaligned read avoids alignment assumptions.
            let value = unsafe { buffer.cast::<WTS_CONNECTSTATE_CLASS>().read_unaligned() };
            if value == WTSActive {
                Some(true)
            } else if [
                WTSConnected,
                WTSConnectQuery,
                WTSShadow,
                WTSDisconnected,
                WTSIdle,
                WTSListen,
                WTSReset,
                WTSDown,
                WTSInit,
            ]
            .contains(&value)
            {
                Some(false)
            } else {
                None
            }
        } else {
            None
        };
    if !buffer.is_null() {
        // SAFETY: free only the buffer returned by this WTS call, exactly once.
        unsafe { WTSFreeMemory(buffer.cast()) };
    }
    result
}

fn default_input_desktop() -> Option<bool> {
    // SAFETY: read-only, non-inherited input desktop handle; no switching or hooks.
    let desktop = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) };
    if desktop.is_null() {
        return None;
    }
    let mut name = [0u16; 128];
    let mut needed = 0;
    // SAFETY: bounded UTF-16 output buffer with its exact byte capacity; only UOI_NAME is read.
    let ok = unsafe {
        GetUserObjectInformationW(
            desktop,
            UOI_NAME,
            name.as_mut_ptr().cast(),
            size_of_val(&name) as u32,
            &mut needed,
        )
    };
    // SAFETY: close only this successful OpenInputDesktop handle once; close failure invalidates proof.
    let closed = unsafe { CloseDesktop(desktop) };
    if ok == 0
        || closed == 0
        || needed == 0
        || needed as usize > size_of_val(&name)
        || needed % 2 != 0
    {
        return None;
    }
    let end = name[..needed as usize / 2]
        .iter()
        .position(|unit| *unit == 0)?;
    Some(name[..end] == [68, 101, 102, 97, 117, 108, 116])
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const OPEN: Reading = Reading {
        console: Some(true),
        connected: Some(true),
        default_desktop: Some(true),
    };

    #[test]
    fn session_native_delivery_closes_gate_before_sleep_and_wake_callbacks() {
        let gate = IoGate::new();
        gate.set_engine_permits(true);
        let shared = Arc::new(Shared::new(gate.clone()));
        shared.complete(shared.begin_read(), OPEN);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let events = captured.clone();
        let observed = gate.clone();
        shared
            .subscribe(Arc::new(move |event| {
                events.lock().unwrap().push((event, observed.is_open()))
            }))
            .unwrap();
        shared.signal(Signal::Sleep);
        shared.signal(Signal::Wake);
        let events = captured.lock().unwrap();
        assert!(events[0].1);
        assert!(events[1..].iter().all(|(_, open)| !open));
    }

    #[test]
    fn session_native_observer_exit_closes_gate_and_emits_unknown() {
        let gate = IoGate::new();
        gate.set_engine_permits(true);
        let shared = Arc::new(Shared::new(gate.clone()));
        shared.complete(shared.begin_read(), OPEN);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let events = captured.clone();
        shared
            .subscribe(Arc::new(move |event| events.lock().unwrap().push(event)))
            .unwrap();
        drop(ThreadExit(shared.clone()));
        assert!(!gate.is_open());
        assert_eq!(shared.state(), UNKNOWN);
        assert_eq!(
            captured.lock().unwrap().last(),
            Some(&SessionEvent::State(UNKNOWN))
        );
    }

    #[test]
    fn session_native_cleanup_closes_before_releasing_owned_resources() {
        let (shared, gate) = opened();
        // No native resource exists and every cleanup flag is false: this fake makes no
        // Win32 call, but exercises the production Native::drop safety boundary.
        drop(Native {
            context: Context {
                shared: shared.clone(),
                session: 0,
                alive: Rc::new(Cell::new(true)),
            },
            class: Vec::new(),
            instance: null_mut(),
            registered_class: false,
            window: null_mut(),
            wts: false,
            power: 0,
        });
        assert!(!gate.is_open());
        assert_eq!(shared.state(), UNKNOWN);
    }

    #[test]
    fn session_native_loss_closes_before_waiting_for_event_serialization() {
        let (shared, gate) = opened();
        let emission = shared.emission.lock().unwrap();
        let observed = shared.clone();
        let worker = thread::spawn(move || observed.signal(Signal::Lost));
        let deadline = Instant::now() + Duration::from_secs(1);
        while gate.is_open() && Instant::now() < deadline {
            thread::yield_now();
        }
        let closed_before_delivery = !gate.is_open();
        drop(emission);
        worker.join().unwrap();
        assert!(closed_before_delivery);
        assert_eq!(shared.state(), UNKNOWN);
    }

    fn opened() -> (Arc<Shared>, Arc<IoGate>) {
        let gate = IoGate::new();
        gate.set_engine_permits(true);
        let shared = Arc::new(Shared::new(gate.clone()));
        shared.complete(shared.begin_read(), OPEN);
        assert!(gate.is_open());
        (shared, gate)
    }

    #[test]
    fn session_native_stale_read_and_api_failure_never_grant_io() {
        let (shared, gate) = opened();
        let before = shared.begin_read();
        shared.signal(Signal::Lock);
        shared.complete(before, OPEN);
        assert!(!gate.is_open());
        shared.signal(Signal::Unlock);
        shared.complete(shared.begin_read(), Reading::UNKNOWN);
        assert_eq!(shared.state(), UNKNOWN);
        assert!(!gate.is_open());
        shared.complete(shared.begin_read(), OPEN);
        assert!(gate.is_open());
        gate.set_engine_permits(false);
        shared.complete(shared.begin_read(), OPEN);
        assert!(!gate.is_open());
    }

    #[test]
    fn session_native_subscribe_is_initial_once_and_preserves_original_sink() {
        let (shared, _) = opened();
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        shared
            .subscribe(Arc::new(move |event| captured.lock().unwrap().push(event)))
            .unwrap();
        assert_eq!(events.lock().unwrap().len(), 1);
        assert!(matches!(events.lock().unwrap()[0], SessionEvent::State(_)));
        assert!(
            shared
                .subscribe(Arc::new(|_| panic!(
                    "replacement sink must not be installed"
                )))
                .is_err()
        );
        shared.complete(shared.begin_read(), Reading::UNKNOWN);
        assert_eq!(events.lock().unwrap().len(), 2);
    }

    #[test]
    fn session_native_drop_closes_and_stops_events_even_if_a_read_finishes_later() {
        let (shared, gate) = opened();
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        shared
            .subscribe(Arc::new(move |event| captured.lock().unwrap().push(event)))
            .unwrap();
        let old = shared.begin_read();
        drop(WindowsSession {
            shared: shared.clone(),
            worker: None,
            watchdog: None,
        });
        shared.complete(old, OPEN);
        shared.signal(Signal::Wake);
        assert!(!gate.is_open());
        assert_eq!(shared.state(), UNKNOWN);
        assert_eq!(events.lock().unwrap().len(), 1);
    }

    #[test]
    fn session_native_sink_panic_closes_and_cannot_unwind_into_win32() {
        let (shared, gate) = opened();
        assert!(
            shared
                .subscribe(Arc::new(|_| panic!("injected sink failure")))
                .is_err()
        );
        assert!(!gate.is_open());
        assert_eq!(shared.state(), UNKNOWN);
        shared.complete(shared.begin_read(), OPEN);
        assert!(!gate.is_open());
    }

    #[test]
    fn session_native_sink_can_read_state_without_a_data_lock_deadlock() {
        let (shared, _) = opened();
        let weak = Arc::downgrade(&shared);
        shared
            .subscribe(Arc::new(move |event| {
                if let SessionEvent::State(state) = event {
                    assert_eq!(weak.upgrade().unwrap().state(), state);
                }
            }))
            .unwrap();
        shared.signal(Signal::Sleep);
        shared.signal(Signal::Wake);
        shared.complete(shared.begin_read(), OPEN);
    }

    #[test]
    fn session_native_watchdog_closes_on_a_stalled_observer() {
        let (shared, gate) = opened();
        *shared.progress.lock().unwrap() =
            Instant::now().checked_sub(Duration::from_secs(3)).unwrap();
        shared.watchdog();
        assert!(!gate.is_open());
        assert_eq!(shared.state(), UNKNOWN);
        shared.complete(shared.begin_read(), OPEN);
        assert!(!gate.is_open());
    }

    #[test]
    fn session_native_poison_is_unknown_and_observer_exit_reports_loss() {
        let (shared, gate) = opened();
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        shared
            .subscribe(Arc::new(move |event| captured.lock().unwrap().push(event)))
            .unwrap();
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let _data = shared.data.lock().unwrap();
            panic!("injected observation poison");
        }));
        assert_eq!(shared.state(), UNKNOWN);
        drop(ThreadExit(shared.clone()));
        assert!(!gate.is_open());
        assert_eq!(
            events.lock().unwrap().last(),
            Some(&SessionEvent::State(UNKNOWN))
        );
    }

    #[test]
    fn session_native_drop_joins_responsive_owned_workers() {
        let (shared, gate) = opened();
        let observed = shared.clone();
        let worker = thread::spawn(move || {
            let _exit = ThreadExit(observed.clone());
            while !observed.stopped.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(10));
            }
        });
        let watched = shared.clone();
        let watchdog = thread::spawn(move || watched.watchdog());
        let before = Instant::now();
        drop(WindowsSession {
            shared,
            worker: Some(worker),
            watchdog: Some(watchdog),
        });
        assert!(before.elapsed() < OBSERVATION_BOUND);
        assert!(!gate.is_open());
    }
}
