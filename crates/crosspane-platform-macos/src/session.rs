//! Read-only WindowServer session observation. Notifications close the gate synchronously;
//! only a dictionary read begun after the last notification can reopen it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::Duration;

use crosspane_platform::{
    EventSink, IoGate, LockState, PlatformError, SessionEvent, SessionEvents, SessionState,
};
use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceScreensDidSleepNotification,
    NSWorkspaceScreensDidWakeNotification, NSWorkspaceSessionDidBecomeActiveNotification,
    NSWorkspaceSessionDidResignActiveNotification, NSWorkspaceWillSleepNotification,
};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString, CFType};
use objc2_core_graphics::CGSessionCopyCurrentDictionary;
use objc2_foundation::{
    NSDistributedNotificationCenter, NSNotification, NSNotificationCenter,
    NSNotificationSuspensionBehavior, NSObject, NSObjectProtocol, ns_string,
};

use crate::main_thread::spawn_on_main;

const POLL: Duration = Duration::from_millis(500);
const UNKNOWN: SessionState = SessionState {
    lock: LockState::Unknown,
    active: None,
};
// CoreGraphics/CGSession.h defines kCGSessionOnConsoleKey as this CFSTR, not an exported symbol.
const ON_CONSOLE: &str = "kCGSSessionOnConsoleKey";
const SCREEN_LOCKED: &str = "CGSSessionScreenIsLocked";

fn decide(fields: Option<(Option<bool>, Option<bool>)>) -> SessionState {
    let Some((active, locked)) = fields else {
        return UNKNOWN;
    };
    SessionState {
        active,
        lock: if locked == Some(true) {
            LockState::Locked
        } else if active == Some(true) {
            LockState::Unlocked
        } else {
            LockState::Unknown
        },
    }
}

fn boolean(dictionary: &CFDictionary<CFString, CFType>, key: &str) -> Result<Option<bool>, ()> {
    dictionary
        .get(&CFString::from_str(key))
        .map(|value| {
            value
                .downcast::<CFBoolean>()
                .map(|value| value.value())
                .map_err(|_| ())
        })
        .transpose()
}

fn read_state() -> SessionState {
    let Some(dictionary) = CGSessionCopyCurrentDictionary() else {
        return UNKNOWN;
    };
    // SAFETY: CGSession.h specifies string keys and CF object values. Each boolean is downcast
    // separately, so missing or incorrectly typed values cannot be interpreted as permission.
    let dictionary: CFRetained<CFDictionary<CFString, CFType>> =
        unsafe { CFRetained::cast_unchecked(dictionary) };
    match (
        boolean(&dictionary, ON_CONSOLE),
        boolean(&dictionary, SCREEN_LOCKED),
    ) {
        (Ok(active), Ok(locked)) => decide(Some((active, locked))),
        _ => UNKNOWN,
    }
}

#[derive(Clone, Copy, Debug)]
enum Signal {
    Locked,
    Inactive,
    Sleep,
    Wake,
    ScreensSleep,
    ScreensWake,
    Refresh,
}

struct Data {
    state: SessionState,
    sink: Option<Arc<dyn EventSink<SessionEvent>>>,
    generation: u64,
    refresh: bool,
    sleeping: bool,
    screens_asleep: bool,
    stopped: bool,
    observers_installed: bool,
}

struct Shared {
    gate: Arc<IoGate>,
    data: Mutex<Data>,
    changed: Condvar,
    failed: AtomicBool,
}

impl Shared {
    fn new(gate: Arc<IoGate>) -> Self {
        gate.set_session_permits(false);
        Self {
            gate,
            data: Mutex::new(Data {
                state: UNKNOWN,
                sink: None,
                generation: 0,
                refresh: false,
                sleeping: false,
                screens_asleep: false,
                stopped: false,
                observers_installed: false,
            }),
            changed: Condvar::new(),
            failed: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Data> {
        match self.data.lock() {
            Ok(data) => data,
            Err(poison) => {
                self.failed.store(true, Ordering::Release);
                self.gate.set_session_permits(false);
                let mut data = poison.into_inner();
                data.state = UNKNOWN;
                data
            }
        }
    }

    fn running(&self, data: &Data) -> bool {
        !data.stopped && !self.failed.load(Ordering::Acquire)
    }

    // The mutex serializes both gate updates and sends from the worker and notification thread.
    fn state_event(&self, data: &mut Data, state: SessionState, force: bool) {
        self.gate.set_session_permits(state.permits_io());
        let changed = data.state != state;
        data.state = state;
        if (changed || force)
            && let Some(sink) = &data.sink
        {
            sink.send(SessionEvent::State(state));
        }
    }

    fn refresh(&self, source: impl FnOnce() -> SessionState) {
        let generation = {
            let data = self.lock();
            if !self.running(&data) {
                return;
            }
            data.generation
        };
        // Never hold the serialization lock across an OS read. A notification can invalidate it.
        let state = source();
        let mut data = self.lock();
        if !self.running(&data) || generation != data.generation {
            return;
        }
        let state = if data.sleeping || data.screens_asleep {
            UNKNOWN
        } else {
            state
        };
        let force = data.refresh;
        data.refresh = false;
        self.state_event(&mut data, state, force);
    }

    fn notify(&self, signal: Signal) {
        let mut data = self.lock();
        if !self.running(&data) {
            return;
        }
        self.gate.set_session_permits(false);
        data.generation = data.generation.wrapping_add(1);
        data.refresh = true;
        let state = match signal {
            Signal::Locked => SessionState {
                lock: LockState::Locked,
                active: data.state.active,
            },
            Signal::Inactive => SessionState {
                lock: if data.state.lock == LockState::Locked {
                    LockState::Locked
                } else {
                    LockState::Unknown
                },
                active: Some(false),
            },
            Signal::Sleep => {
                data.sleeping = true;
                UNKNOWN
            }
            Signal::Wake => {
                data.sleeping = false;
                UNKNOWN
            }
            Signal::ScreensSleep => {
                data.screens_asleep = true;
                UNKNOWN
            }
            Signal::ScreensWake => {
                data.screens_asleep = false;
                UNKNOWN
            }
            Signal::Refresh => UNKNOWN,
        };
        if let Some(sink) = &data.sink {
            match signal {
                Signal::Sleep => sink.send(SessionEvent::WillSleep),
                Signal::Wake => sink.send(SessionEvent::Woke),
                _ => {}
            }
        }
        self.state_event(&mut data, state, false);
        self.changed.notify_one();
    }

    fn guarded(&self, action: impl FnOnce()) {
        if catch_unwind(AssertUnwindSafe(|| {
            let _close_on_panic = CloseOnPanic(self);
            action();
        }))
        .is_err()
        {
            let mut data = self.lock();
            if !data.stopped
                && let Some(sink) = &data.sink
            {
                // A broken sink must not unwind through an Objective-C callback or panic again.
                if catch_unwind(AssertUnwindSafe(|| sink.send(SessionEvent::State(UNKNOWN))))
                    .is_err()
                {
                    data.sink = None;
                }
            }
            tracing::warn!("macOS session observation panicked; I/O gate closed");
        }
    }

    fn poll(&self) {
        let mut warned = false;
        loop {
            self.refresh(read_state);
            let data = self.lock();
            if !self.running(&data) {
                return;
            }
            if data.refresh {
                continue;
            }
            match self.changed.wait_timeout(data, POLL) {
                Ok((data, _)) => {
                    if !self.running(&data) {
                        return;
                    }
                    if !data.observers_installed && !warned {
                        tracing::warn!(
                            "macOS session observers unavailable (no AppKit loop); relying on 500 ms polling"
                        );
                        warned = true;
                    }
                }
                Err(_) => {
                    self.failed.store(true, Ordering::Release);
                    self.gate.set_session_permits(false);
                    self.lock().state = UNKNOWN;
                    return;
                }
            }
        }
    }
}

struct CloseOnPanic<'a>(&'a Shared);

impl Drop for CloseOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.failed.store(true, Ordering::Release);
            let mut data = self.0.lock();
            self.0.gate.set_session_permits(false);
            data.state = UNKNOWN;
            self.0.changed.notify_all();
        }
    }
}

/// A Send handle; native observers are owned and removed exclusively on the AppKit main thread.
pub struct MacSession {
    shared: Arc<Shared>,
}

impl fmt::Debug for MacSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MacSession")
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

impl MacSession {
    pub fn new(gate: Arc<IoGate>) -> Result<MacSession, PlatformError> {
        let session = Self {
            shared: Arc::new(Shared::new(gate)),
        };
        session
            .shared
            .guarded(|| session.shared.refresh(read_state));
        let worker = session.shared.clone();
        std::thread::Builder::new()
            .name("mac-session-poll".into())
            .spawn(move || worker.guarded(|| worker.poll()))
            .map_err(|error| {
                PlatformError::Backend(format!("spawn session poll thread: {error}"))
            })?;
        let shared = session.shared.clone();
        spawn_on_main(move |mtm| shared.guarded(|| install_observers(&shared, mtm)));
        Ok(session)
    }
}

impl SessionEvents for MacSession {
    fn state(&self) -> SessionState {
        self.shared.lock().state
    }

    fn subscribe(&mut self, sink: Arc<dyn EventSink<SessionEvent>>) -> Result<(), PlatformError> {
        let _close_on_panic = CloseOnPanic(&self.shared);
        let mut data = self.shared.lock();
        if data.sink.is_some() {
            return Err(PlatformError::Backend(
                "SessionEvents::subscribe called twice".into(),
            ));
        }
        if !self.shared.running(&data) {
            return Err(PlatformError::Backend("session observation failed".into()));
        }
        data.sink = Some(sink);
        let state = data.state;
        self.shared.state_event(&mut data, state, true);
        Ok(())
    }
}

impl Drop for MacSession {
    fn drop(&mut self) {
        {
            let mut data = self.shared.lock();
            data.stopped = true;
            self.shared.gate.set_session_permits(false);
            data.sink = None;
            self.shared.changed.notify_all();
        }
        // Keeping this Arc alive also prevents address reuse before the main-thread cleanup.
        let shared = self.shared.clone();
        spawn_on_main(move |_| {
            OBSERVERS.with(|observers| {
                observers.borrow_mut().remove(&Arc::as_ptr(&shared));
            });
        });
    }
}

struct ObserverIvars {
    shared: Weak<Shared>,
    signal: Signal,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; this class has no Drop implementation.
    #[unsafe(super = NSObject)]
    #[name = "CrosspaneSessionObserver"]
    #[thread_kind = MainThreadOnly]
    #[ivars = ObserverIvars]
    struct Observer;

    // SAFETY: NSObjectProtocol adds no requirements.
    unsafe impl NSObjectProtocol for Observer {}

    impl Observer {
        // SAFETY: Matches the notification center's void(id, SEL, NSNotification *) callback.
        #[unsafe(method(sessionChanged:))]
        fn changed(&self, _notification: &NSNotification) {
            if let Some(shared) = self.ivars().shared.upgrade() {
                shared.guarded(|| shared.notify(self.ivars().signal));
            }
        }
    }
);

impl Observer {
    fn new(shared: &Arc<Shared>, signal: Signal, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ObserverIvars {
            shared: Arc::downgrade(shared),
            signal,
        });
        // SAFETY: NSObject's init takes no arguments, and the Rust ivars are initialized.
        unsafe { msg_send![super(this), init] }
    }
}

struct Observers {
    distributed: Retained<NSDistributedNotificationCenter>,
    workspace: Retained<NSNotificationCenter>,
    tokens: Vec<Retained<Observer>>,
}

impl Drop for Observers {
    fn drop(&mut self) {
        for observer in &self.tokens {
            // SAFETY: These are the retained objects registered with these centers, on main.
            unsafe {
                self.distributed.removeObserver(observer);
                self.workspace.removeObserver(observer);
            }
        }
    }
}

thread_local! {
    static OBSERVERS: RefCell<HashMap<*const Shared, Observers>> = RefCell::new(HashMap::new());
}

fn install_observers(shared: &Arc<Shared>, mtm: MainThreadMarker) {
    if !shared.running(&shared.lock()) {
        return;
    }
    let mut observers = Observers {
        distributed: NSDistributedNotificationCenter::defaultCenter(),
        workspace: NSWorkspace::sharedWorkspace().notificationCenter(),
        tokens: Vec::new(),
    };
    for (name, signal) in [
        (ns_string!("com.apple.screenIsLocked"), Signal::Locked),
        (ns_string!("com.apple.screenIsUnlocked"), Signal::Refresh),
    ] {
        let observer = Observer::new(shared, signal, mtm);
        // SAFETY: Observer implements this selector with the required notification signature.
        // No sender filter; DeliverImmediately avoids coalescing safety notifications.
        unsafe {
            observers
                .distributed
                .addObserver_selector_name_object_suspensionBehavior(
                    &observer,
                    sel!(sessionChanged:),
                    Some(name),
                    None,
                    NSNotificationSuspensionBehavior::DeliverImmediately,
                );
        }
        observers.tokens.push(observer);
    }
    // SAFETY: These are immutable notification-name constants exported by AppKit.
    let names = unsafe {
        [
            (
                NSWorkspaceSessionDidResignActiveNotification,
                Signal::Inactive,
            ),
            (
                NSWorkspaceSessionDidBecomeActiveNotification,
                Signal::Refresh,
            ),
            (NSWorkspaceWillSleepNotification, Signal::Sleep),
            (NSWorkspaceDidWakeNotification, Signal::Wake),
            (NSWorkspaceScreensDidSleepNotification, Signal::ScreensSleep),
            (NSWorkspaceScreensDidWakeNotification, Signal::ScreensWake),
        ]
    };
    for (name, signal) in names {
        let observer = Observer::new(shared, signal, mtm);
        // SAFETY: Same selector and retained observer as above; no sender filter.
        unsafe {
            observers.workspace.addObserver_selector_name_object(
                &observer,
                sel!(sessionChanged:),
                Some(name),
                None,
            );
        }
        observers.tokens.push(observer);
    }
    let mut data = shared.lock();
    if shared.running(&data) {
        OBSERVERS.with(|all| {
            all.borrow_mut().insert(Arc::as_ptr(shared), observers);
        });
        data.observers_installed = true;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const UNLOCKED: SessionState = SessionState {
        lock: LockState::Unlocked,
        active: Some(true),
    };
    type Events = Arc<Mutex<Vec<(SessionEvent, bool)>>>;

    fn recording_session() -> (MacSession, Arc<IoGate>, Events) {
        let gate = IoGate::new();
        gate.set_engine_permits(true);
        let mut session = MacSession {
            shared: Arc::new(Shared::new(gate.clone())),
        };
        session.shared.refresh(|| UNLOCKED);
        let events = Arc::new(Mutex::new(Vec::new()));
        let recorded = events.clone();
        let observed_gate = gate.clone();
        session
            .subscribe(Arc::new(move |event| {
                recorded
                    .lock()
                    .unwrap()
                    .push((event, observed_gate.is_open()));
            }))
            .unwrap();
        (session, gate, events)
    }

    #[test]
    fn dictionary_fields_decide_state() {
        assert_eq!(decide(None), UNKNOWN);
        for active in [None, Some(false), Some(true)] {
            for locked in [None, Some(false), Some(true)] {
                let expected_lock = match (locked, active) {
                    (Some(true), _) => LockState::Locked,
                    (_, Some(true)) => LockState::Unlocked,
                    _ => LockState::Unknown,
                };
                let state = decide(Some((active, locked)));
                assert_eq!(
                    state,
                    SessionState {
                        lock: expected_lock,
                        active
                    }
                );
                assert_eq!(
                    state.permits_io(),
                    active == Some(true) && locked != Some(true)
                );
            }
        }
        let key = CFString::from_str(ON_CONSOLE);
        let wrong_value = CFString::from_str("not a boolean");
        let dictionary =
            CFDictionary::<CFString, CFType>::from_slices(&[&key], &[wrong_value.as_ref()]);
        assert_eq!(boolean(&dictionary, ON_CONSOLE), Err(()));
        assert_eq!(boolean(&dictionary, SCREEN_LOCKED), Ok(None));
        let dictionary = CFDictionary::<CFString, CFType>::from_slices(
            &[&key],
            &[CFBoolean::new(true).as_ref()],
        );
        assert_eq!(boolean(&dictionary, ON_CONSOLE), Ok(Some(true)));
    }

    #[test]
    fn notification_gate_sequencing() {
        let (session, gate, events) = recording_session();
        let shared = &session.shared;
        assert_eq!(
            events.lock().unwrap()[0],
            (SessionEvent::State(UNLOCKED), true)
        );
        shared.notify(Signal::Locked);
        assert!(!gate.is_open());
        assert_eq!(shared.lock().state.lock, LockState::Locked);
        assert!(!events.lock().unwrap().last().unwrap().1);
        shared.refresh(|| UNLOCKED);
        assert!(gate.is_open());
        shared.notify(Signal::Inactive);
        assert_eq!(shared.lock().state.active, Some(false));
        assert!(!gate.is_open());
        shared.notify(Signal::Refresh); // Unlock or become-active is only a request to read.
        assert!(!gate.is_open());
        shared.refresh(|| UNLOCKED);
        assert!(gate.is_open());
        shared.notify(Signal::Sleep);
        assert!(!gate.is_open());
        shared.refresh(|| UNLOCKED);
        assert!(
            !gate.is_open(),
            "polling cannot reopen during announced sleep"
        );
        shared.notify(Signal::Wake);
        assert!(!gate.is_open());
        shared.refresh(|| UNLOCKED);
        assert!(gate.is_open());
        shared.notify(Signal::ScreensSleep);
        shared.notify(Signal::Sleep);
        shared.notify(Signal::Wake);
        shared.refresh(|| UNLOCKED);
        assert!(
            !gate.is_open(),
            "machine wake alone does not imply screens woke"
        );
        shared.notify(Signal::ScreensWake);
        assert!(!gate.is_open());
        shared.refresh(|| UNLOCKED);
        assert!(gate.is_open());
        shared.refresh(|| UNKNOWN);
        assert!(!gate.is_open());
        let events = events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|(event, _)| *event == SessionEvent::WillSleep)
        );
        assert!(events.iter().any(|(event, _)| *event == SessionEvent::Woke));
        for (event, open) in events.iter() {
            assert_eq!(
                *open,
                match event {
                    SessionEvent::State(state) => state.permits_io(),
                    _ => false,
                }
            );
        }
    }

    #[test]
    fn wake_rejects_read_started_before_notification() {
        let (session, gate, events) = recording_session();
        let shared = session.shared.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        let read = std::thread::spawn(move || {
            shared.refresh(|| {
                started_tx.send(()).unwrap();
                finish_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                UNLOCKED
            })
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        session.shared.notify(Signal::Wake);
        let count_after_wake = events.lock().unwrap().len();
        assert!(!gate.is_open());
        finish_tx.send(()).unwrap();
        read.join().unwrap();
        assert!(!gate.is_open());
        assert_eq!(events.lock().unwrap().len(), count_after_wake);
        session.shared.refresh(|| UNLOCKED);
        assert_eq!(
            events.lock().unwrap().last(),
            Some(&(SessionEvent::State(UNLOCKED), true))
        );
    }

    #[test]
    fn observation_panic_and_drop_fail_closed() {
        let (session, gate, events) = recording_session();
        session
            .shared
            .guarded(|| session.shared.refresh(|| panic!("injected source failure")));
        assert!(!gate.is_open());
        assert_eq!(session.state(), UNKNOWN);
        assert_eq!(
            events.lock().unwrap().last(),
            Some(&(SessionEvent::State(UNKNOWN), false))
        );
        session
            .shared
            .refresh(|| panic!("a failed observer must not read again"));
        let shared = session.shared.clone();
        let count = events.lock().unwrap().len();
        drop(session);
        shared.notify(Signal::Wake);
        shared.refresh(|| UNLOCKED);
        assert!(!gate.is_open());
        assert_eq!(events.lock().unwrap().len(), count);
        let (session, gate, _) = recording_session();
        assert!(gate.is_open());
        drop(session);
        assert!(!gate.is_open());
    }
}
