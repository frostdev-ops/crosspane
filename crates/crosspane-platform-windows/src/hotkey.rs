//! Release-only panic chord using an owned Raw Input keyboard recipient.
//!
//! The application MUST exclusively own keyboard Raw Input registration from construction
//! THROUGH Drop. Existing registrations are refused; replacement is terminal. Another keyboard
//! backend in this process needs a future shared broker, not a second registration.
//!
//! A non-null RAWINPUTHEADER device is an observation predicate, NOT documented proof against
//! SendInput. GetAsyncKeyState seeding also has no physical provenance and conflates up/failure.
//! These limitations are accepted for a panic chord that can only end sharing, never start or
//! extend it. Do not reuse this source for positive authorization. The frozen HotkeyEvent has no
//! asynchronous health variant: loss is metadata-only and later calls fail synchronously;
//! W1.5b must address that integration gap. No physical release is fabricated on observer loss.

#![allow(unsafe_code)]

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};
use std::{
    cell::RefCell,
    fmt,
    mem::size_of,
    ptr::{null, null_mut},
    thread::{self, JoinHandle},
};

use crosspane_platform::{Chord, EventSink, GlobalHotkeys, HotkeyEvent, PlatformError};
use crosspane_types::{
    hid::{HidUsage, ScanPrefix, hid_to_windows},
    time::MonoTime,
};
use windows_sys::Win32::{
    Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{
        Input::{
            KeyboardAndMouse::{GetAsyncKeyState, MAPVK_VSC_TO_VK_EX, MapVirtualKeyW},
            *,
        },
        WindowsAndMessaging::*,
    },
};

use crate::model::hotkey::{HotkeyModel, RawKey, registration_available, registration_owned};

const BOUND: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(500);
static CLAIMED: AtomicBool = AtomicBool::new(false);

fn now() -> MonoTime {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    MonoTime::from_nanos(
        u64::try_from(EPOCH.get_or_init(Instant::now).elapsed().as_nanos()).unwrap_or(u64::MAX),
    )
}

struct Data {
    model: HotkeyModel,
    sink: Option<Arc<dyn EventSink<HotkeyEvent>>>,
}

struct Shared {
    data: Mutex<Data>,
    emission: Mutex<()>,
    stopped: AtomicBool,
    progress: Mutex<Instant>,
}

impl Shared {
    fn new() -> Self {
        Self {
            data: Mutex::new(Data {
                model: HotkeyModel::default(),
                sink: None,
            }),
            emission: Mutex::new(()),
            stopped: AtomicBool::new(false),
            progress: Mutex::new(Instant::now()),
        }
    }

    fn unavailable() -> PlatformError {
        PlatformError::Unsupported("Windows raw keyboard observer unavailable")
    }

    fn check(&self) -> Result<(), PlatformError> {
        if self.stopped.load(Ordering::Acquire) {
            Err(Self::unavailable())
        } else {
            Ok(())
        }
    }

    fn configure(
        &self,
        chord: &Chord,
        down: &[HidUsage],
        at: MonoTime,
    ) -> Result<(), PlatformError> {
        let _emission = self.emission.lock().map_err(|_| Self::unavailable())?;
        self.check()?;
        let mut data = self.data.lock().map_err(|_| Self::unavailable())?;
        let events = data.model.configure(chord, down, at)?;
        let sink = data.sink.clone();
        drop(data);
        self.send(sink, events)
    }

    fn subscribe(
        &self,
        sink: Arc<dyn EventSink<HotkeyEvent>>,
        down: &[HidUsage],
        at: MonoTime,
    ) -> Result<(), PlatformError> {
        let _emission = self.emission.lock().map_err(|_| Self::unavailable())?;
        self.check()?;
        let mut data = self.data.lock().map_err(|_| Self::unavailable())?;
        data.model.seed_initial(down, at)?;
        let event = data.model.subscribe(at)?;
        data.sink = Some(sink.clone());
        drop(data);
        self.send(Some(sink), vec![event])
    }

    fn raw(&self, key: RawKey, at: MonoTime) -> Result<(), PlatformError> {
        let _emission = self.emission.lock().map_err(|_| Self::unavailable())?;
        self.check()?;
        let mut data = self.data.lock().map_err(|_| Self::unavailable())?;
        let events = data.model.raw(key, at).into_iter().collect();
        data.model.available()?;
        let sink = data.sink.clone();
        drop(data);
        self.send(sink, events)
    }

    fn send(
        &self,
        sink: Option<Arc<dyn EventSink<HotkeyEvent>>>,
        events: Vec<HotkeyEvent>,
    ) -> Result<(), PlatformError> {
        if let Some(sink) = sink {
            for event in events {
                self.check()?;
                if catch_unwind(AssertUnwindSafe(|| sink.send(event))).is_err() {
                    self.lose("sink failure");
                    return Err(Self::unavailable());
                }
            }
        }
        self.check()
    }

    fn lose(&self, cause: &'static str) {
        if !self.stopped.swap(true, Ordering::AcqRel) {
            // Static operation/cause metadata only, never keyboard/device/chord contents.
            eprintln!("Windows panic chord observer stopped: {cause}");
        }
        if let Ok(mut data) = self.data.lock() {
            data.model.lose();
            data.sink = None;
        }
    }

    fn stop(&self) {
        self.lose("backend dropped");
        // Wait out any already-entered nonblocking sink callback. Later sends check stopped.
        let _emission = self.emission.lock();
    }

    fn touch(&self) {
        if let Ok(mut progress) = self.progress.lock() {
            *progress = Instant::now();
        } else {
            self.lose("progress state poisoned");
        }
    }

    fn watch(&self) {
        while self.check().is_ok() {
            if self
                .progress
                .lock()
                .map_or(true, |progress| progress.elapsed() > BOUND)
            {
                self.lose("observer stalled");
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

enum Operation {
    Configure(Chord),
    Subscribe(Arc<dyn EventSink<HotkeyEvent>>),
}
struct Command {
    operation: Operation,
    reply: mpsc::SyncSender<Result<(), PlatformError>>,
}

/// Application-exclusive keyboard Raw Input owner. Never consumes legacy keys.
pub struct WindowsHotkeys {
    shared: Arc<Shared>,
    commands: mpsc::SyncSender<Command>,
    worker: Option<JoinHandle<()>>,
    watchdog: Option<JoinHandle<()>>,
}

impl fmt::Debug for WindowsHotkeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowsHotkeys")
            .field("stopped", &self.shared.stopped.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

struct WorkerExit(Arc<Shared>);
impl Drop for WorkerExit {
    fn drop(&mut self) {
        self.0.lose("window thread exited");
        // Native lives inside the worker and has already been dropped, even on panic.
        CLAIMED.store(false, Ordering::Release);
    }
}

impl WindowsHotkeys {
    /// Refuses any pre-existing process keyboard registration. This contract must remain
    /// exclusive until native cleanup finishes, including after a bounded Drop times out.
    pub fn new() -> Result<Self, PlatformError> {
        if CLAIMED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(PlatformError::Unsupported(
                "keyboard Raw Input already owned",
            ));
        }
        let shared = Arc::new(Shared::new());
        let (commands, receiver) = mpsc::sync_channel(8);
        let (ready, startup) = mpsc::sync_channel(1);
        let observed = shared.clone();
        let worker = match thread::Builder::new()
            .name("crosspane-win-hotkey".into())
            .spawn(move || {
                let _exit = WorkerExit(observed.clone());
                match Native::new(observed) {
                    Ok(native) => {
                        if ready.send(Ok(())).is_ok() {
                            native.run(receiver);
                        }
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                }
            }) {
            Ok(worker) => worker,
            Err(_) => {
                CLAIMED.store(false, Ordering::Release);
                return Err(PlatformError::Backend(
                    "spawn Windows hotkey observer failed".into(),
                ));
            }
        };
        let mut backend = Self {
            shared,
            commands,
            worker: Some(worker),
            watchdog: None,
        };
        let watched = backend.shared.clone();
        backend.watchdog = Some(
            thread::Builder::new()
                .name("crosspane-hotkey-watch".into())
                .spawn(move || watched.watch())
                .map_err(|_| {
                    PlatformError::Backend("spawn Windows hotkey watchdog failed".into())
                })?,
        );
        startup.recv_timeout(BOUND).map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => PlatformError::Timeout,
            mpsc::RecvTimeoutError::Disconnected => Shared::unavailable(),
        })??;
        backend.shared.check()?;
        Ok(backend)
    }

    fn request(&self, operation: Operation) -> Result<(), PlatformError> {
        self.shared.check()?;
        let (reply, result) = mpsc::sync_channel(1);
        self.commands
            .try_send(Command { operation, reply })
            .map_err(|_| Shared::unavailable())?;
        match result.recv_timeout(BOUND) {
            Ok(result) => result,
            Err(error) => {
                // No queued command can become a successful late configuration after timeout.
                self.shared.lose("command unavailable");
                Err(match error {
                    mpsc::RecvTimeoutError::Timeout => PlatformError::Timeout,
                    mpsc::RecvTimeoutError::Disconnected => Shared::unavailable(),
                })
            }
        }
    }
}

impl GlobalHotkeys for WindowsHotkeys {
    fn set_chord(&mut self, chord: &Chord) -> Result<(), PlatformError> {
        self.request(Operation::Configure(chord.clone()))
    }
    fn subscribe(&mut self, sink: Arc<dyn EventSink<HotkeyEvent>>) -> Result<(), PlatformError> {
        self.request(Operation::Subscribe(sink))
    }
}

impl Drop for WindowsHotkeys {
    fn drop(&mut self) {
        self.shared.stop();
        let deadline = Instant::now() + BOUND;
        for worker in [&mut self.worker, &mut self.watchdog] {
            if let Some(worker) = worker.take() {
                while !worker.is_finished() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
                if worker.is_finished() {
                    let _ = worker.join();
                }
                // Win32 calls cannot safely be killed. A stalled worker retains its own window,
                // registration and process claim until it returns and performs native cleanup.
            }
        }
    }
}

thread_local! { static CONTEXT: RefCell<Option<Arc<Shared>>> = const { RefCell::new(None) }; }

fn os_error(operation: &'static str) -> PlatformError {
    // SAFETY: retrieves a scalar last-error value on this thread, no input contents.
    PlatformError::Backend(format!("{operation}: Win32 error {}", unsafe {
        GetLastError()
    }))
}

fn keyboard_targets() -> Result<Vec<usize>, PlatformError> {
    let mut count = 0;
    // SAFETY: null buffer requests the count into a valid scalar with exact element size.
    if unsafe {
        GetRegisteredRawInputDevices(null_mut(), &mut count, size_of::<RAWINPUTDEVICE>() as u32)
    } == u32::MAX
    {
        return Err(os_error("query Raw Input registrations"));
    }
    if count > 64 {
        return Err(PlatformError::Unsupported(
            "Raw Input registration inventory exceeds bound",
        ));
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut entries = vec![RAWINPUTDEVICE::default(); count as usize];
    // SAFETY: initialized aligned array has count elements; the API reports any concurrent growth
    // as insufficient space. No unbounded retry or registration mutation occurs here.
    let copied = unsafe {
        GetRegisteredRawInputDevices(
            entries.as_mut_ptr(),
            &mut count,
            size_of::<RAWINPUTDEVICE>() as u32,
        )
    };
    if copied == u32::MAX || copied as usize > entries.len() {
        return Err(os_error("read Raw Input registrations"));
    }
    let mut targets = Vec::new();
    for entry in entries.into_iter().take(copied as usize) {
        if entry.usUsagePage == 1 && (entry.usUsage == 6 || entry.usUsage == 0) {
            // A page-wide registration could also interfere. Only our exact INPUTSINK flags
            // qualify as continued ownership; other registrations are still refused at startup.
            targets.push(if entry.usUsage == 6 && entry.dwFlags == RIDEV_INPUTSINK {
                entry.hwndTarget as usize
            } else {
                0
            });
        }
    }
    Ok(targets)
}

// Injectable native decision boundary: existing keyboard ownership must cause ZERO mutation.
fn admit_registration(
    target: usize,
    mut query: impl FnMut() -> Result<Vec<usize>, PlatformError>,
    register: impl FnOnce(usize) -> Result<(), PlatformError>,
) -> Result<(), PlatformError> {
    if target == 0 || !registration_available(&query()?) {
        return Err(PlatformError::Unsupported(
            "pre-existing keyboard Raw Input registration",
        ));
    }
    register(target)
}

fn check_registration(
    shared: &Shared,
    target: usize,
    result: Result<Vec<usize>, PlatformError>,
) -> Result<(), PlatformError> {
    match result {
        Ok(targets) if registration_owned(&targets, target) => shared.check(),
        _ => {
            shared.lose("keyboard registration lost or unreadable");
            Err(Shared::unavailable())
        }
    }
}

fn remove_registration(
    target: usize,
    query: impl FnOnce() -> Result<Vec<usize>, PlatformError>,
    remove: impl FnOnce() -> Result<(), PlatformError>,
) -> Result<(), PlatformError> {
    if registration_owned(&query()?, target) {
        remove()
    } else {
        Ok(())
    }
}

fn snapshot(chord: &Chord) -> Result<Vec<HidUsage>, PlatformError> {
    let mut down = Vec::new();
    for usage in std::iter::once(chord.key).chain(chord.modifiers.iter().copied()) {
        let scan =
            hid_to_windows(usage).ok_or(PlatformError::Unsupported("unmapped hotkey chord"))?;
        let prefix = match scan.prefix {
            ScanPrefix::None => 0,
            ScanPrefix::E0 => 0xe000,
            ScanPrefix::E1 => 0xe100,
        };
        // SAFETY: maps a scalar set-1 scan code to a virtual key, never to a character.
        let vk = unsafe { MapVirtualKeyW(prefix | u32::from(scan.code), MAPVK_VSC_TO_VK_EX) };
        if vk == 0 || vk > 255 {
            return Err(PlatformError::Unsupported(
                "hotkey async-state mapping unavailable",
            ));
        }
        // SAFETY: reads only the requested chord key's current high bit, never typed contents.
        if unsafe { GetAsyncKeyState(vk as i32) } < 0 {
            down.push(usage);
        }
    }
    Ok(down)
}

fn read_key(handle: HRAWINPUT) -> Result<Option<RawKey>, PlatformError> {
    let mut bytes = 0;
    // SAFETY: original WM_INPUT handle, size query only with exact RAWINPUTHEADER size.
    if unsafe {
        GetRawInputData(
            handle,
            RID_INPUT,
            null_mut(),
            &mut bytes,
            size_of::<RAWINPUTHEADER>() as u32,
        )
    } == u32::MAX
    {
        return Err(os_error("size Raw Input report"));
    }
    if bytes < (size_of::<RAWINPUTHEADER>() + size_of::<RAWKEYBOARD>()) as u32
        || bytes as usize > size_of::<RAWINPUT>()
    {
        return Ok(None);
    }
    let mut input = RAWINPUT::default();
    let requested = bytes;
    // SAFETY: RAWINPUT is initialized and DWORD-aligned; the checked buffer is large enough.
    let copied = unsafe {
        GetRawInputData(
            handle,
            RID_INPUT,
            (&mut input as *mut RAWINPUT).cast(),
            &mut bytes,
            size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if copied == u32::MAX {
        return Err(os_error("read Raw Input report"));
    }
    Ok(decode_key(&input, copied, requested, bytes))
}

fn decode_key(input: &RAWINPUT, copied: u32, requested: u32, bytes: u32) -> Option<RawKey> {
    if copied != requested
        || bytes != requested
        || input.header.dwSize != requested
        || input.header.dwType != RIM_TYPEKEYBOARD
        || input.header.hDevice.is_null()
    {
        return None;
    }
    // SAFETY: complete checked keyboard packet selects the keyboard union member.
    let key = unsafe { input.data.keyboard };
    if key.Reserved != 0
        || !matches!(
            key.Message,
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP
        )
        || (key.Flags & 1 != 0) != matches!(key.Message, WM_KEYUP | WM_SYSKEYUP)
    {
        return None;
    }
    Some(RawKey {
        device: input.header.hDevice as usize,
        make_code: key.MakeCode,
        flags: key.Flags,
    })
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    let shared = CONTEXT.with(|context| {
        context
            .try_borrow()
            .ok()
            .and_then(|context| context.clone())
    });
    if let Some(shared) = shared {
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            if message == WM_INPUT && shared.check().is_ok() {
                match read_key(lp as HRAWINPUT) {
                    Ok(Some(key)) => {
                        if shared.raw(key, now()).is_err() {
                            shared.lose("raw report delivery failed");
                        }
                    }
                    Ok(None) => {}
                    Err(_) => shared.lose("raw report read failed"),
                }
            } else if message == WM_NCDESTROY {
                shared.lose("observer window destroyed");
            }
        }));
        if outcome.is_err() {
            shared.lose("window callback failed");
        }
    }
    // SAFETY: forwards original arguments to the default handler. WM_INPUT foreground cleanup
    // specifically requires DefWindowProc; raw observation never consumes legacy keyboard input.
    unsafe { DefWindowProcW(window, message, wp, lp) }
}

struct Native {
    shared: Arc<Shared>,
    class: Vec<u16>,
    instance: HINSTANCE,
    class_registered: bool,
    window: HWND,
    registered: bool,
}

impl Native {
    fn new(shared: Arc<Shared>) -> Result<Self, PlatformError> {
        if !registration_available(&keyboard_targets()?) {
            return Err(PlatformError::Unsupported(
                "pre-existing keyboard Raw Input registration",
            ));
        }
        shared.check()?;
        // SAFETY: borrows our module and reads our worker ID for an owned unique class name.
        let (instance, tid) = unsafe { (GetModuleHandleW(null()), GetCurrentThreadId()) };
        if instance.is_null() {
            return Err(os_error("hotkey module"));
        }
        CONTEXT.with(|context| *context.borrow_mut() = Some(shared.clone()));
        let mut native = Self {
            shared,
            class: format!("CrosspaneHotkey.{tid}")
                .encode_utf16()
                .chain([0])
                .collect(),
            instance,
            class_registered: false,
            window: null_mut(),
            registered: false,
        };
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: native.class.as_ptr(),
            ..Default::default()
        };
        // SAFETY: owned worker/class, stable UTF-16 name and callback ABI. Message-only window
        // receives targeted WM_INPUT; it has no visible style and is never shown or activated.
        unsafe {
            if RegisterClassW(&class) == 0 {
                return Err(os_error("register hotkey window class"));
            }
            native.class_registered = true;
            native.window = CreateWindowExW(
                0,
                native.class.as_ptr(),
                native.class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                null_mut(),
                instance,
                null(),
            );
        }
        if native.window.is_null() {
            return Err(os_error("create hotkey observer"));
        }
        native.shared.check()?;
        admit_registration(native.window as usize, keyboard_targets, |target| {
            let device = RAWINPUTDEVICE {
                usUsagePage: 1,
                usUsage: 6,
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: target as HWND,
            };
            // SAFETY: exact single keyboard TLC, owned target; exclusive process contract admitted.
            if unsafe { RegisterRawInputDevices(&device, 1, size_of::<RAWINPUTDEVICE>() as u32) }
                == 0
            {
                Err(os_error("register keyboard Raw Input"))
            } else {
                Ok(())
            }
        })?;
        native.registered = true;
        native.ownership()?;
        Ok(native)
    }

    fn ownership(&self) -> Result<(), PlatformError> {
        check_registration(&self.shared, self.window as usize, keyboard_targets())
    }

    fn command(&self, operation: Operation) -> Result<(), PlatformError> {
        self.shared.check()?;
        self.ownership()?;
        match operation {
            Operation::Configure(chord) => self.shared.configure(&chord, &snapshot(&chord)?, now()),
            Operation::Subscribe(sink) => {
                let chord = self
                    .shared
                    .data
                    .lock()
                    .map_err(|_| Shared::unavailable())?
                    .model
                    .chord()
                    .cloned()
                    .ok_or(PlatformError::Unsupported("hotkey chord not configured"))?;
                self.shared.subscribe(sink, &snapshot(&chord)?, now())
            }
        }
    }

    fn run(&self, commands: mpsc::Receiver<Command>) {
        let mut next_poll = Instant::now();
        while self.shared.check().is_ok() {
            if Instant::now() >= next_poll {
                if self.ownership().is_err() {
                    self.shared.lose("keyboard registration lost");
                    break;
                }
                next_poll = Instant::now() + POLL;
            }
            for _ in 0..8 {
                match commands.try_recv() {
                    Ok(command) => {
                        let result = self.command(command.operation);
                        let _ = command.reply.send(result);
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.shared.lose("command channel closed");
                        return;
                    }
                }
            }
            for _ in 0..64 {
                let mut message = MSG::default();
                // SAFETY: owns this thread's message queue and initialized MSG; nonblocking read.
                if unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) } == 0 {
                    break;
                }
                if message.message == WM_QUIT {
                    self.shared.lose("message thread quit");
                    return;
                }
                // SAFETY: exact retrieved message, owned thread; dispatch preserves default handling.
                unsafe {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                if self.shared.check().is_err() {
                    break;
                }
            }
            self.shared.touch();
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        if self.registered {
            let cleaned = remove_registration(self.window as usize, keyboard_targets, || {
                let device = RAWINPUTDEVICE {
                    usUsagePage: 1,
                    usUsage: 6,
                    dwFlags: RIDEV_REMOVE,
                    hwndTarget: null_mut(),
                };
                // SAFETY: process-exclusive keyboard owner through Drop; removal requires NULL target.
                // After replacement we deliberately skip this call so another recipient stays intact.
                if unsafe {
                    RegisterRawInputDevices(&device, 1, size_of::<RAWINPUTDEVICE>() as u32)
                } == 0
                {
                    Err(os_error("remove keyboard Raw Input"))
                } else {
                    Ok(())
                }
            });
            if cleaned.is_err() {
                eprintln!("Windows panic chord cleanup: unregister not verified");
            }
        }
        // SAFETY: only this worker's own HWND/class are destroyed, exactly once on its thread.
        unsafe {
            if !self.window.is_null() && DestroyWindow(self.window) == 0 {
                eprintln!("Windows panic chord cleanup: window destroy failed");
            }
            if self.class_registered && UnregisterClassW(self.class.as_ptr(), self.instance) == 0 {
                eprintln!("Windows panic chord cleanup: class removal failed");
            }
        }
        CONTEXT.with(|context| *context.borrow_mut() = None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn chord() -> Chord {
        Chord {
            modifiers: vec![HidUsage::keyboard(0xe0)],
            key: HidUsage::keyboard(0x29),
        }
    }
    fn key(shared: &Shared, code: u16, flags: u16) -> Result<(), PlatformError> {
        shared.raw(
            RawKey {
                device: 1,
                make_code: code,
                flags,
            },
            MonoTime::ZERO,
        )
    }

    #[test]
    fn hotkey_initial_event_precedes_exact_physical_pair() {
        let shared = Shared::new();
        shared.configure(&chord(), &[], MonoTime::ZERO).unwrap();
        let (tx, rx) = mpsc::channel();
        shared
            .subscribe(
                Arc::new(move |event| {
                    tx.send(event).unwrap();
                }),
                &[],
                MonoTime::ZERO,
            )
            .unwrap();
        key(&shared, 0x1d, 0).unwrap();
        key(&shared, 1, 0).unwrap();
        key(&shared, 1, 0).unwrap();
        key(&shared, 1, 1).unwrap();
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            vec![
                HotkeyEvent::Released { at: MonoTime::ZERO },
                HotkeyEvent::Pressed { at: MonoTime::ZERO },
                HotkeyEvent::Released { at: MonoTime::ZERO }
            ]
        );
    }

    #[test]
    fn hotkey_observer_loss_refuses_later_calls_without_false_release() {
        let shared = Shared::new();
        shared.configure(&chord(), &[], MonoTime::ZERO).unwrap();
        let (tx, rx) = mpsc::channel();
        shared
            .subscribe(
                Arc::new(move |event| {
                    tx.send(event).unwrap();
                }),
                &[],
                MonoTime::ZERO,
            )
            .unwrap();
        key(&shared, 0x1d, 0).unwrap();
        key(&shared, 1, 0).unwrap();
        assert_eq!(rx.try_iter().count(), 2);
        shared.lose("fake window exit");
        assert!(key(&shared, 1, 1).is_err());
        assert!(shared.configure(&chord(), &[], MonoTime::ZERO).is_err());
        assert_eq!(rx.try_iter().count(), 0);
    }

    #[test]
    fn hotkey_broken_sink_is_terminal_and_never_unwinds() {
        let shared = Shared::new();
        shared.configure(&chord(), &[], MonoTime::ZERO).unwrap();
        assert!(
            shared
                .subscribe(
                    Arc::new(|_| panic!("fake sink failure")),
                    &[],
                    MonoTime::ZERO
                )
                .is_err()
        );
        assert!(shared.configure(&chord(), &[], MonoTime::ZERO).is_err());
    }

    #[test]
    fn hotkey_registration_existing_or_unreadable_has_zero_mutations() {
        for query in [Ok(vec![9]), Ok(vec![0]), Err(PlatformError::Timeout)] {
            let mutations = std::cell::Cell::new(0);
            let mut query = Some(query);
            assert!(
                admit_registration(
                    7,
                    || query.take().unwrap(),
                    |_| {
                        mutations.set(mutations.get() + 1);
                        Ok(())
                    }
                )
                .is_err()
            );
            assert_eq!(mutations.get(), 0);
        }
        assert!(
            admit_registration(
                7,
                || Ok(vec![]),
                |_| Err(PlatformError::Backend("fake registration error".into()))
            )
            .is_err()
        );
    }

    #[test]
    fn hotkey_registration_loss_is_terminal_without_physical_events() {
        let shared = Shared::new();
        shared.configure(&chord(), &[], MonoTime::ZERO).unwrap();
        assert!(check_registration(&shared, 7, Ok(vec![7])).is_ok());
        assert!(check_registration(&shared, 7, Ok(vec![9])).is_err());
        assert!(shared.configure(&chord(), &[], MonoTime::ZERO).is_err());
        let failed = Shared::new();
        assert!(check_registration(&failed, 7, Err(PlatformError::Timeout)).is_err());
        assert!(failed.check().is_err());
    }

    #[test]
    fn hotkey_cleanup_removes_only_still_owned_registration() {
        for query in [Ok(vec![9]), Ok(vec![]), Err(PlatformError::Timeout)] {
            let removed = std::cell::Cell::new(false);
            let _ = remove_registration(
                7,
                || query,
                || {
                    removed.set(true);
                    Ok(())
                },
            );
            assert!(!removed.get());
        }
        let removed = std::cell::Cell::new(false);
        remove_registration(
            7,
            || Ok(vec![7]),
            || {
                removed.set(true);
                Ok(())
            },
        )
        .unwrap();
        assert!(removed.get());
    }

    #[test]
    fn hotkey_packet_validation_rejects_wrong_size_type_device_and_direction() {
        let bytes = (size_of::<RAWINPUTHEADER>() + size_of::<RAWKEYBOARD>()) as u32;
        let mut input = RAWINPUT::default();
        input.header.dwSize = bytes;
        input.header.dwType = RIM_TYPEKEYBOARD;
        input.header.hDevice = 1usize as _;
        input.data.keyboard = RAWKEYBOARD {
            MakeCode: 1,
            Message: WM_KEYDOWN,
            ..Default::default()
        };
        assert!(decode_key(&input, bytes, bytes, bytes).is_some());
        assert!(decode_key(&input, bytes - 1, bytes, bytes).is_none());
        assert!(decode_key(&input, bytes, bytes, bytes + 1).is_none());
        input.header.dwSize -= 1;
        assert!(decode_key(&input, bytes, bytes, bytes).is_none());
        input.header.dwSize = bytes;
        input.header.dwType = RIM_TYPEMOUSE;
        assert!(decode_key(&input, bytes, bytes, bytes).is_none());
        input.header.dwType = RIM_TYPEKEYBOARD;
        input.header.hDevice = null_mut();
        assert!(decode_key(&input, bytes, bytes, bytes).is_none());
        input.header.hDevice = 1usize as _;
        input.data.keyboard = RAWKEYBOARD {
            MakeCode: 1,
            Message: WM_KEYUP,
            ..Default::default()
        };
        assert!(decode_key(&input, bytes, bytes, bytes).is_none());
        input.data.keyboard = RAWKEYBOARD {
            MakeCode: 1,
            Flags: 1,
            Message: WM_KEYUP,
            ..Default::default()
        };
        assert!(decode_key(&input, bytes, bytes, bytes).is_some());
    }

    #[test]
    fn hotkey_stalled_worker_and_late_command_are_terminal() {
        let shared = Shared::new();
        *shared.progress.lock().unwrap() = Instant::now() - BOUND - Duration::from_millis(1);
        shared.watch();
        assert!(shared.configure(&chord(), &[], MonoTime::ZERO).is_err());
        assert!(
            shared
                .subscribe(Arc::new(|_| panic!("late callback")), &[], MonoTime::ZERO)
                .is_err()
        );
    }

    #[test]
    fn hotkey_drop_retires_delivery_before_late_reports() {
        let shared = Shared::new();
        shared.configure(&chord(), &[], MonoTime::ZERO).unwrap();
        let (tx, rx) = mpsc::channel();
        shared
            .subscribe(
                Arc::new(move |event| {
                    tx.send(event).unwrap();
                }),
                &[],
                MonoTime::ZERO,
            )
            .unwrap();
        assert_eq!(rx.try_iter().count(), 1);
        shared.stop();
        assert!(key(&shared, 0x1d, 0).is_err());
        assert!(key(&shared, 1, 0).is_err());
        assert_eq!(rx.try_iter().count(), 0);
    }
}
