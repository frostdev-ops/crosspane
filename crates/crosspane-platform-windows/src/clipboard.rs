//! Clipboard part 1. One message-window owner performs every native data call. Win32 delayed
//! rendering may hold OpenClipboard for up to 30 seconds; the caller's two-second timeout cannot
//! shorten that OS wait. A timed-out command retains its reservation and RAII resources until
//! the owner returns; later read/kinds calls refuse immediately rather than accumulating work.
#![allow(unsafe_code)]

use crate::model::clipboard::{self as model, Format, Formats, Snapshot, Watch};
use crosspane_platform::{
    ClipKinds, ClipboardEvent, ClipboardHost, EventSink, IoGate, LocalPasteId, PlatformError,
};
use crosspane_types::ClipKind;
use std::{
    cell::Cell,
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::{null, null_mut},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{GetLastError, HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, SetLastError, WPARAM},
    System::{
        DataExchange::*,
        LibraryLoader::GetModuleHandleW,
        Memory::*,
        Ole::{CF_DIB, CF_DIBV5, CF_UNICODETEXT},
        Threading::GetCurrentThreadId,
    },
    UI::WindowsAndMessaging::*,
};
use zeroize::Zeroizing;

const BOUND: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(10);
type Sink = Arc<dyn EventSink<ClipboardEvent>>;
struct Delivery {
    pending: Option<(Snapshot, usize, u64)>,
    sink: Option<Sink>,
    watch: Watch,
}

struct Shared {
    gate: Arc<IoGate>,
    busy: AtomicBool,
    stopped: AtomicBool,
    dirty: AtomicBool,
    delivery: Mutex<Delivery>,
    wake: Condvar,
    cleaned: AtomicBool,
    native_failed: AtomicBool,
}
impl Shared {
    fn new(gate: Arc<IoGate>) -> Self {
        Self {
            gate,
            busy: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            dirty: AtomicBool::new(true),
            delivery: Mutex::new(Delivery {
                pending: None,
                sink: None,
                watch: Watch::default(),
            }),
            wake: Condvar::new(),
            cleaned: AtomicBool::new(false),
            native_failed: AtomicBool::new(false),
        }
    }
    fn reserve(self: &Arc<Self>) -> Result<Reservation, PlatformError> {
        self.check()?;
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| PlatformError::Timeout)?;
        if let Err(error) = self.check() {
            self.busy.store(false, Ordering::Release);
            return Err(error);
        }
        Ok(Reservation(self.clone()))
    }
    fn check(&self) -> Result<(), PlatformError> {
        if self.stopped.load(Ordering::Acquire) {
            Err(backend("clipboard observer unavailable"))
        } else {
            Ok(())
        }
    }
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        if let Ok(mut delivery) = self.delivery.lock() {
            delivery.pending = None;
            delivery.sink = None;
        }
        self.wake.notify_all();
    }
    fn subscribe(&self, sink: Sink) -> Result<(), PlatformError> {
        self.check()?;
        let mut delivery = self
            .delivery
            .lock()
            .map_err(|_| backend("clipboard delivery poisoned"))?;
        delivery.pending = None;
        delivery.watch = Watch::default();
        delivery.sink = Some(sink);
        Ok(())
    }
    fn queue(&self, snapshot: Snapshot, own: usize, epoch: u64) -> Result<(), PlatformError> {
        self.check()?;
        let mut delivery = self
            .delivery
            .lock()
            .map_err(|_| backend("clipboard delivery poisoned"))?;
        if delivery.sink.is_some() {
            delivery.pending = Some((snapshot, own, epoch));
            self.wake.notify_one();
        }
        Ok(())
    }
    fn next(&self, delivery: &mut Delivery) -> Option<(Sink, ClipKinds)> {
        let (snapshot, own, epoch) = delivery.pending.take()?;
        if self.check().is_err() || !self.gate.is_open() || self.gate.epoch() != epoch {
            // Do not acknowledge an event discarded on close/reopen. The owner reconciles it
            // once under the new epoch even when no further WM_CLIPBOARDUPDATE arrives.
            self.dirty.store(true, Ordering::Release);
            return None;
        }
        let kinds = delivery.watch.observe(snapshot, own, true)?;
        Some((delivery.sink.clone()?, kinds))
    }
    fn deliver(&self) {
        while self.check().is_ok() {
            let next = (|| {
                let mut delivery = self.delivery.lock().map_err(|_| ())?;
                while delivery.pending.is_none() && !self.stopped.load(Ordering::Acquire) {
                    delivery = self.wake.wait(delivery).map_err(|_| ())?;
                }
                Ok::<_, ()>(self.next(&mut delivery))
            })();
            match next {
                Ok(Some((sink, kinds))) => {
                    if catch_unwind(AssertUnwindSafe(|| {
                        sink.send(ClipboardEvent::Changed { kinds })
                    }))
                    .is_err()
                    {
                        eprintln!("Windows clipboard observer: sink failed");
                        self.stop();
                    }
                }
                Ok(None) => {}
                Err(()) => {
                    self.stop();
                    break;
                }
            }
        }
    }
}
struct Reservation(Arc<Shared>);
impl Drop for Reservation {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::Release);
    }
}
struct Permit {
    deadline: Instant,
    epoch: u64,
    abandoned: Arc<AtomicBool>,
}
impl Permit {
    fn check(&self, shared: &Shared) -> Result<(), PlatformError> {
        shared.check()?;
        model::admit(
            shared.gate.is_open(),
            self.epoch,
            shared.gate.epoch(),
            self.abandoned.load(Ordering::Acquire) || Instant::now() >= self.deadline,
        )
    }
}
trait Access {
    fn open(&mut self) -> Result<bool, PlatformError>;
    fn close(&mut self) -> Result<(), PlatformError>;
    fn snapshot(&mut self) -> Result<(Snapshot, Formats), PlatformError>;
    fn data(&mut self, format: Format, limit: usize) -> Result<Zeroizing<Vec<u8>>, PlatformError>;
}
fn read_with(
    port: &mut impl Access,
    shared: &Shared,
    permit: &Permit,
    own: usize,
    kind: ClipKind,
    limit: usize,
) -> Result<Zeroizing<Vec<u8>>, PlatformError> {
    permit.check(shared)?;
    let started = Instant::now();
    loop {
        permit.check(shared)?;
        let delay = model::retry_delay(started.elapsed().as_millis() as u64)?;
        if port.open()? {
            break;
        }
        thread::sleep(Duration::from_millis(delay));
    }
    let mut opened = OpenGuard {
        port,
        active: true,
        shared,
    };
    let result = (|| {
        permit.check(shared)?;
        let (before, formats) = opened.port.snapshot()?;
        let format = formats.select(kind, before.owner == own)?;
        permit.check(shared)?;
        let bytes = opened.port.data(format, limit)?;
        permit.check(shared)?;
        let (after, _) = opened.port.snapshot()?;
        if !model::coherent(
            (before.sequence, before.owner),
            (after.sequence, after.owner),
        ) {
            return Err(PlatformError::Timeout);
        }
        permit.check(shared)?;
        Ok(bytes)
    })();
    let closed = opened.close();
    if closed.is_err() {
        shared.native_failed.store(true, Ordering::Release);
        shared.stop();
    }
    let bytes = result?;
    closed?;
    permit.check(shared)?;
    Ok(bytes)
}
struct OpenGuard<'a, T: Access> {
    port: &'a mut T,
    active: bool,
    shared: &'a Shared,
}
impl<T: Access> OpenGuard<'_, T> {
    fn close(&mut self) -> Result<(), PlatformError> {
        self.active = false;
        self.port.close()
    }
}
impl<T: Access> Drop for OpenGuard<'_, T> {
    fn drop(&mut self) {
        if self.active && self.port.close().is_err() {
            self.shared.native_failed.store(true, Ordering::Release);
            self.shared.stop();
            eprintln!("Windows clipboard unwind close unverified");
        }
    }
}

fn backend(operation: &'static str) -> PlatformError {
    PlatformError::Backend(operation.into())
}
fn os_error(operation: &'static str) -> PlatformError {
    // SAFETY: reads this thread's scalar OS error; no content or foreign format name.
    PlatformError::Backend(format!("{operation}: Win32 error {}", unsafe {
        GetLastError()
    }))
}
enum Operation {
    Kinds,
    Read(ClipKind, usize),
    Subscribe(Sink),
}
enum Reply {
    Kinds(ClipKinds),
    Data(Zeroizing<Vec<u8>>),
    Unit,
}
struct Command {
    operation: Operation,
    permit: Permit,
    reservation: Reservation,
    reply: mpsc::SyncSender<Result<Reply, PlatformError>>,
}

/// Read/watch-only clipboard owner; delayed rendering and promise writing belong to part 2.
pub struct WindowsClipboard {
    shared: Arc<Shared>,
    commands: mpsc::SyncSender<Command>,
    owner: Option<JoinHandle<()>>,
    delivery: Option<JoinHandle<()>>,
}
impl fmt::Debug for WindowsClipboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowsClipboard")
            .field("busy", &self.shared.busy.load(Ordering::Acquire))
            .field("stopped", &self.shared.stopped.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}
struct OwnerExit(Arc<Shared>);
impl Drop for OwnerExit {
    fn drop(&mut self) {
        self.0.stop();
    }
}
impl WindowsClipboard {
    pub fn new(gate: Arc<IoGate>) -> Result<Self, PlatformError> {
        let shared = Arc::new(Shared::new(gate));
        let (commands, receiver) = mpsc::sync_channel(1);
        let (ready, startup) = mpsc::sync_channel(1);
        let mut backend = Self {
            shared: shared.clone(),
            commands,
            owner: None,
            delivery: None,
        };
        let delivering = shared.clone();
        backend.delivery = Some(
            thread::Builder::new()
                .name("crosspane-clip-delivery".into())
                .spawn(move || delivering.deliver())
                .map_err(|_| backend_error_spawn())?,
        );
        backend.owner = Some(
            thread::Builder::new()
                .name("crosspane-clip-owner".into())
                .spawn(move || {
                    let _exit = OwnerExit(shared.clone());
                    match Native::new(shared) {
                        Ok(mut native) => {
                            if ready.send(Ok(())).is_ok() {
                                native.run(receiver);
                            }
                        }
                        Err(error) => {
                            let _ = ready.send(Err(error));
                        }
                    }
                })
                .map_err(|_| backend_error_spawn())?,
        );
        startup
            .recv_timeout(BOUND)
            .map_err(|_| PlatformError::Timeout)??;
        backend.shared.check()?;
        Ok(backend)
    }
    fn request(&self, operation: Operation) -> Result<Reply, PlatformError> {
        self.request_bounded(operation, BOUND)
    }
    fn request_bounded(
        &self,
        operation: Operation,
        bound: Duration,
    ) -> Result<Reply, PlatformError> {
        let reservation = self.shared.reserve()?;
        let abandoned = Arc::new(AtomicBool::new(false));
        let permit = Permit {
            deadline: Instant::now() + bound,
            epoch: self.shared.gate.epoch(),
            abandoned: abandoned.clone(),
        };
        if matches!(operation, Operation::Read(..)) {
            permit.check(&self.shared)?;
        }
        let deadline = permit.deadline;
        let (reply, response) = mpsc::sync_channel(1);
        self.commands
            .try_send(Command {
                operation,
                permit,
                reservation,
                reply,
            })
            .map_err(|_| PlatformError::Timeout)?;
        let result = response.recv_timeout(deadline.saturating_duration_since(Instant::now()));
        match result {
            Ok(result) => {
                self.shared.check()?;
                if Instant::now() >= deadline {
                    return Err(PlatformError::Timeout);
                }
                result
            }
            Err(_) => {
                abandoned.store(true, Ordering::Release);
                Err(PlatformError::Timeout)
            }
        }
    }
    fn finish(&mut self) -> bool {
        self.shared.stop();
        let deadline = Instant::now() + BOUND;
        let mut joined = true;
        for worker in [&mut self.owner, &mut self.delivery] {
            if let Some(worker) = worker.take() {
                while !worker.is_finished() && Instant::now() < deadline {
                    thread::sleep(POLL);
                }
                if worker.is_finished() {
                    joined &= worker.join().is_ok();
                } else {
                    joined = false;
                }
            }
        }
        joined && self.shared.cleaned.load(Ordering::Acquire)
    }
    #[cfg(test)]
    pub(crate) fn stop_verified(mut self) -> bool {
        self.finish()
    }
}
fn backend_error_spawn() -> PlatformError {
    backend("spawn clipboard worker failed")
}
impl ClipboardHost for WindowsClipboard {
    fn subscribe(&mut self, sink: Sink) -> Result<(), PlatformError> {
        match self.request(Operation::Subscribe(sink))? {
            Reply::Unit => Ok(()),
            _ => Err(backend("clipboard reply type mismatch")),
        }
    }
    fn kinds(&self) -> Result<ClipKinds, PlatformError> {
        match self.request(Operation::Kinds)? {
            Reply::Kinds(kinds) => Ok(kinds),
            _ => Err(backend("clipboard reply type mismatch")),
        }
    }
    fn read(&mut self, kind: ClipKind, max_bytes: usize) -> Result<Vec<u8>, PlatformError> {
        if !self.shared.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        let epoch = self.shared.gate.epoch();
        match self.request(Operation::Read(kind, max_bytes))? {
            Reply::Data(mut data) => {
                model::admit(
                    self.shared.gate.is_open(),
                    epoch,
                    self.shared.gate.epoch(),
                    false,
                )?;
                Ok(std::mem::take(&mut *data))
            }
            _ => Err(backend("clipboard reply type mismatch")),
        }
    }
    fn promise(&mut self, _offer: u64, _kinds: ClipKinds) -> Result<(), PlatformError> {
        if !self.shared.gate.is_open() {
            Err(PlatformError::Locked)
        } else {
            Err(PlatformError::Unsupported(
                "Windows clipboard promise part 2",
            ))
        }
    }
    fn fulfil(&mut self, _paste: LocalPasteId, data: Option<Vec<u8>>) {
        let _data = data.map(Zeroizing::new);
    }
    fn withdraw(&mut self, _offer: u64) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported(
            "Windows clipboard promise part 2",
        ))
    }
}
impl Drop for WindowsClipboard {
    fn drop(&mut self) {
        if !self.finish() {
            eprintln!("Windows clipboard cleanup unverified: owner or delivery retained");
        }
    }
}

thread_local! { static SIGNAL: Cell<*const AtomicBool> = const { Cell::new(null()) }; }
unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    if message == WM_CLIPBOARDUPDATE {
        SIGNAL.with(|signal| {
            let pointer = signal.get();
            if !pointer.is_null() {
                // SAFETY: owner installs a stable Arc-backed atomic before window creation and
                // retains it through DestroyWindow; callback performs only this O(1) signal.
                unsafe {
                    (*pointer).store(true, Ordering::Release);
                }
            }
        });
    }
    // SAFETY: original message arguments, no foreign window fields or content read.
    unsafe { DefWindowProcW(window, message, wp, lp) }
}
struct Native {
    shared: Arc<Shared>,
    window: HWND,
    class: Vec<u16>,
    instance: HINSTANCE,
    registered: bool,
    listener: bool,
    png: u32,
}
impl Native {
    fn new(shared: Arc<Shared>) -> Result<Self, PlatformError> {
        // SAFETY: reads own loaded module and scalar own thread identity.
        let (instance, id) = unsafe { (GetModuleHandleW(null()), GetCurrentThreadId()) };
        if instance.is_null() {
            return Err(os_error("clipboard module"));
        }
        let class: Vec<u16> = format!("CrosspaneClip{}-{id}", std::process::id())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut native = Self {
            shared,
            window: null_mut(),
            class,
            instance,
            registered: false,
            listener: false,
            png: 0,
        };
        SIGNAL.with(|signal| signal.set(&native.shared.dirty));
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: native.class.as_ptr(),
            ..Default::default()
        };
        // SAFETY: class strings and module live through unregister; callback context is installed.
        if unsafe { RegisterClassW(&wc) } == 0 {
            return Err(os_error("register clipboard class"));
        }
        native.registered = true;
        // SAFETY: never-shown owned message-only window; no activation, foreign HWND or input.
        native.window = unsafe {
            CreateWindowExW(
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
            )
        };
        if native.window.is_null() {
            return Err(os_error("create clipboard observer"));
        }
        // SAFETY: registers standard PNG name, not a foreign format name or content query.
        native.png = unsafe { RegisterClipboardFormatW([80, 78, 71, 0].as_ptr()) };
        if native.png == 0 {
            return Err(os_error("register PNG format"));
        }
        // SAFETY: owned live message window, balanced RemoveClipboardFormatListener on owner.
        if unsafe { AddClipboardFormatListener(native.window) } == 0 {
            return Err(os_error("register clipboard listener"));
        }
        native.listener = true;
        Ok(native)
    }
    fn port(&self) -> WinAccess {
        WinAccess {
            window: self.window,
            png: self.png,
            shared: self.shared.clone(),
        }
    }
    fn observe(&mut self) -> Result<(), PlatformError> {
        let epoch = self.shared.gate.epoch();
        let (snapshot, _) = self.port().snapshot()?;
        if self.shared.gate.is_open() {
            self.shared.queue(snapshot, self.window as usize, epoch)?;
        }
        Ok(())
    }
    fn command(&mut self, command: &Command) -> Result<Reply, PlatformError> {
        self.shared.check()?;
        if command.permit.abandoned.load(Ordering::Acquire)
            || Instant::now() >= command.permit.deadline
        {
            return Err(PlatformError::Timeout);
        }
        match &command.operation {
            Operation::Read(kind, limit) => read_with(
                &mut self.port(),
                &self.shared,
                &command.permit,
                self.window as usize,
                *kind,
                *limit,
            )
            .map(Reply::Data),
            Operation::Kinds => self
                .port()
                .snapshot()
                .map(|(snapshot, _)| Reply::Kinds(snapshot.kinds)),
            Operation::Subscribe(sink) => {
                self.shared.subscribe(sink.clone())?;
                self.observe()?;
                Ok(Reply::Unit)
            }
        }
    }
    fn run(&mut self, commands: mpsc::Receiver<Command>) {
        let mut last_epoch = self.shared.gate.epoch();
        let mut metadata_warned = false;
        while self.shared.check().is_ok() {
            for _ in 0..64 {
                let mut message = MSG::default();
                // SAFETY: nonblocking drain of own thread queue; dispatch original messages.
                if unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) } == 0 {
                    break;
                }
                if message.message == WM_QUIT {
                    return;
                }
                // SAFETY: exact retrieved owned-thread message; standard processing.
                unsafe {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            if let Ok(command) = commands.try_recv() {
                let result = self.command(&command);
                let _ = command.reply.send(result);
                // Reservation is held through the native call and late-result send/discard.
                drop(command.reservation);
            }
            let epoch = self.shared.gate.epoch();
            if self.shared.dirty.swap(false, Ordering::AcqRel) || epoch != last_epoch {
                match self.observe() {
                    Err(_) => {
                        self.shared.dirty.store(true, Ordering::Release);
                        if !metadata_warned {
                            eprintln!("Windows clipboard metadata unavailable");
                            metadata_warned = true;
                        }
                    }
                    Ok(()) => {
                        metadata_warned = false;
                    }
                }
            }
            last_epoch = epoch;
            thread::sleep(POLL);
        }
    }
}
impl Drop for Native {
    fn drop(&mut self) {
        let mut cleaned = true;
        // SAFETY: all cleanup occurs on the creating thread, only for owned native resources.
        unsafe {
            if self.listener {
                cleaned &= RemoveClipboardFormatListener(self.window) != 0;
            }
            if !self.window.is_null() {
                cleaned &= DestroyWindow(self.window) != 0;
            }
            if self.registered {
                cleaned &= UnregisterClassW(self.class.as_ptr(), self.instance) != 0;
            }
        }
        SIGNAL.with(|signal| signal.set(null()));
        cleaned &= !self.shared.native_failed.load(Ordering::Acquire);
        self.shared.cleaned.store(cleaned, Ordering::Release);
        if !cleaned {
            eprintln!("Windows clipboard native cleanup unverified");
        }
    }
}
struct WinAccess {
    window: HWND,
    png: u32,
    shared: Arc<Shared>,
}
impl Access for WinAccess {
    fn open(&mut self) -> Result<bool, PlatformError> {
        // SAFETY: opens only for this owned observer; every success has owner-thread RAII close.
        Ok(unsafe { OpenClipboard(self.window) } != 0)
    }
    fn close(&mut self) -> Result<(), PlatformError> {
        // SAFETY: this Access has exactly one successful open, on the current owner thread.
        if unsafe { CloseClipboard() } == 0 {
            Err(os_error("close clipboard"))
        } else {
            Ok(())
        }
    }
    fn snapshot(&mut self) -> Result<(Snapshot, Formats), PlatformError> {
        // SAFETY: metadata-only scalar queries; these do not request delayed content rendering.
        unsafe {
            let before = (GetClipboardSequenceNumber(), GetClipboardOwner() as usize);
            let formats = Formats {
                text: IsClipboardFormatAvailable(u32::from(CF_UNICODETEXT)) != 0,
                png: IsClipboardFormatAvailable(self.png) != 0,
                dib_v5: IsClipboardFormatAvailable(u32::from(CF_DIBV5)) != 0,
                dib: IsClipboardFormatAvailable(u32::from(CF_DIB)) != 0,
            };
            let after = (GetClipboardSequenceNumber(), GetClipboardOwner() as usize);
            if before.0 == 0 {
                return Err(backend("clipboard sequence unavailable"));
            }
            if !model::coherent(before, after) {
                return Err(PlatformError::Timeout);
            }
            Ok((
                Snapshot {
                    sequence: before.0,
                    owner: before.1,
                    kinds: formats.kinds(),
                },
                formats,
            ))
        }
    }
    fn data(&mut self, format: Format, limit: usize) -> Result<Zeroizing<Vec<u8>>, PlatformError> {
        #[cfg(test)]
        probe_admit()?;
        let format_id = match format {
            Format::Text => u32::from(CF_UNICODETEXT),
            Format::Png => self.png,
            Format::DibV5 => u32::from(CF_DIBV5),
            Format::Dib => u32::from(CF_DIB),
        };
        #[cfg(test)]
        PROBE_DATA_REQUESTS.fetch_add(1, Ordering::AcqRel);
        // SAFETY: called exactly once while OpenClipboard is held. OS retains handle ownership;
        // delayed rendering may block here, but the command/window/guard remain owned throughout.
        let handle = unsafe {
            SetLastError(0);
            GetClipboardData(format_id)
        };
        if handle.is_null() {
            // SAFETY: scalar failure distinction on this same thread.
            return Err(if unsafe { GetLastError() } == 0 {
                PlatformError::NotFound
            } else {
                os_error("get clipboard data")
            });
        }
        let memory = handle as HGLOBAL;
        // SAFETY: the supported formats are OS-owned HGLOBAL; size is queried before borrowing.
        let size = unsafe {
            SetLastError(0);
            GlobalSize(memory)
        };
        if size == 0 {
            // SAFETY: scalar error after this thread's size query distinguishes empty/failure.
            return Err(if unsafe { GetLastError() } == 0 {
                PlatformError::NotFound
            } else {
                os_error("size clipboard memory")
            });
        }
        if size > isize::MAX as usize
            || (format == Format::Png && size > limit.min(model::IMAGE_CAP))
        {
            return Err(PlatformError::TooLarge);
        }
        // SAFETY: locks only the OS-owned HGLOBAL while clipboard remains open; no ownership transfer.
        let pointer = unsafe { GlobalLock(memory) };
        if pointer.is_null() {
            return Err(os_error("lock clipboard memory"));
        }
        let _lock = Locked(memory, self.shared.clone());
        // SAFETY: GlobalSize gives the readable allocation; GlobalLock remains alive until after
        // conversion, and the Open guard outlives this lock. No pointer escapes or native retry.
        let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), size) };
        match format {
            Format::Text => model::text(bytes, limit),
            Format::Png => model::png(bytes, limit),
            Format::DibV5 | Format::Dib => model::dib_png(bytes, limit),
        }
        .map(Zeroizing::new)
    }
}
struct Locked(HGLOBAL, Arc<Shared>);
impl Drop for Locked {
    fn drop(&mut self) {
        // SAFETY: balances exactly our GlobalLock; zero with last-error zero is successful final unlock.
        unsafe {
            SetLastError(0);
            if GlobalUnlock(self.0) == 0 && GetLastError() != 0 {
                self.1.native_failed.store(true, Ordering::Release);
                self.1.stop();
                eprintln!("Windows clipboard memory unlock unverified");
            }
        }
    }
}
#[cfg(test)]
static PROBE_FIXTURE: Mutex<Option<(usize, u32)>> = Mutex::new(None);
#[cfg(test)]
static PROBE_DATA_REQUESTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn probe_data_requests() -> usize {
    PROBE_DATA_REQUESTS.load(Ordering::Acquire)
}
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn set_probe_fixture(owner: usize, sequence: u32) {
    if let Ok(mut fixture) = PROBE_FIXTURE.lock() {
        *fixture = Some((owner, sequence));
    }
}
#[cfg(test)]
fn probe_admit() -> Result<(), PlatformError> {
    let expected = PROBE_FIXTURE
        .lock()
        .map_err(|_| backend("probe admission poisoned"))?;
    let Some((owner, sequence)) = *expected else {
        return Err(PlatformError::Unsupported(
            "native clipboard probe not admitted",
        ));
    };
    // SAFETY: executed only under this owner thread's OpenClipboard, checking metadata before
    // any GetClipboardData. The test fixture authenticates the exact live HWND and sequence.
    let observed = unsafe {
        (
            GetClipboardOwner() as usize,
            GetClipboardSequenceNumber(),
            IsWindow(owner as HWND) != 0,
        )
    };
    if probe_matches((owner, sequence), observed) {
        Ok(())
    } else {
        Err(PlatformError::Unsupported("clipboard fixture replaced"))
    }
}
#[cfg(test)]
fn probe_matches(expected: (usize, u32), observed: (usize, u32, bool)) -> bool {
    expected.0 != 0 && expected.1 != 0 && observed.2 && expected == (observed.0, observed.1)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::{sync::atomic::AtomicUsize, time::Duration};
    fn shared() -> Arc<Shared> {
        let gate = Arc::new(IoGate::default());
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        Arc::new(Shared::new(gate))
    }
    fn permit(shared: &Shared) -> Permit {
        Permit {
            deadline: Instant::now() + Duration::from_secs(2),
            epoch: shared.gate.epoch(),
            abandoned: Arc::new(AtomicBool::new(false)),
        }
    }
    struct Fake {
        opens: usize,
        closes: usize,
        reads: usize,
        snapshots: usize,
        fail_data: bool,
        gate_on_data: Option<Arc<IoGate>>,
        race: bool,
        panic_data: bool,
        close_fails: bool,
    }
    impl Fake {
        fn new() -> Self {
            Self {
                opens: 0,
                closes: 0,
                reads: 0,
                snapshots: 0,
                fail_data: false,
                gate_on_data: None,
                race: false,
                panic_data: false,
                close_fails: false,
            }
        }
    }
    impl Access for Fake {
        fn open(&mut self) -> Result<bool, PlatformError> {
            self.opens += 1;
            Ok(true)
        }
        fn close(&mut self) -> Result<(), PlatformError> {
            self.closes += 1;
            if self.close_fails {
                Err(backend("fake close failure"))
            } else {
                Ok(())
            }
        }
        fn snapshot(&mut self) -> Result<(Snapshot, Formats), PlatformError> {
            self.snapshots += 1;
            Ok((
                Snapshot {
                    sequence: if self.race && self.snapshots > 1 {
                        2
                    } else {
                        1
                    },
                    owner: 2,
                    kinds: crosspane_platform::ClipKinds {
                        text: true,
                        image: false,
                    },
                },
                Formats {
                    text: true,
                    ..Default::default()
                },
            ))
        }
        fn data(&mut self, _: Format, _: usize) -> Result<Zeroizing<Vec<u8>>, PlatformError> {
            self.reads += 1;
            assert!(!self.panic_data, "fake data panic");
            if let Some(gate) = &self.gate_on_data {
                gate.set_session_permits(false);
                gate.set_session_permits(true);
            }
            if self.fail_data {
                Err(PlatformError::Backend("fake native failure".into()))
            } else {
                Ok(Zeroizing::new(vec![1]))
            }
        }
    }
    #[test]
    fn clipboard_inflight_read_and_kinds_refuse_without_queue_and_release_on_owner_exit() {
        let shared = shared();
        let held = shared.reserve().unwrap();
        assert!(matches!(shared.reserve(), Err(PlatformError::Timeout)));
        assert!(matches!(shared.reserve(), Err(PlatformError::Timeout)));
        drop(held);
        assert!(shared.reserve().is_ok());
    }
    #[test]
    fn clipboard_probe_replacement_unknown_sequence_and_dead_fixture_refuse_before_data() {
        assert!(probe_matches((1, 2), (1, 2, true)));
        for expected in [(0, 2), (1, 0), (2, 2), (1, 3)] {
            assert!(!probe_matches(expected, (1, 2, true)));
        }
        assert!(!probe_matches((1, 2), (1, 2, false)));
    }
    #[test]
    fn clipboard_expired_abandoned_and_gate_changed_command_never_requests_data() {
        let shared = shared();
        for reason in 0..3 {
            let mut permit = permit(&shared);
            if reason == 0 {
                permit.deadline = Instant::now();
            }
            if reason == 1 {
                permit.abandoned.store(true, Ordering::Release);
            }
            if reason == 2 {
                shared.gate.set_session_permits(false);
                shared.gate.set_session_permits(true);
            }
            let mut fake = Fake::new();
            assert!(read_with(&mut fake, &shared, &permit, 1, ClipKind::Text, 8).is_err());
            assert_eq!(fake.reads, 0);
            assert_eq!(fake.opens, 0);
        }
    }
    #[test]
    fn clipboard_data_failure_closes_native_open_without_retry() {
        let shared = shared();
        let mut fake = Fake::new();
        fake.fail_data = true;
        assert!(read_with(&mut fake, &shared, &permit(&shared), 1, ClipKind::Text, 8).is_err());
        assert_eq!((fake.opens, fake.closes, fake.reads), (1, 1, 1));
    }
    #[test]
    fn clipboard_late_gate_reopen_and_replacement_discard_bytes() {
        let shared = shared();
        for race in [false, true] {
            let mut fake = Fake::new();
            fake.race = race;
            if !race {
                fake.gate_on_data = Some(shared.gate.clone());
            }
            assert!(read_with(&mut fake, &shared, &permit(&shared), 1, ClipKind::Text, 8).is_err());
            assert_eq!((fake.opens, fake.closes, fake.reads), (1, 1, 1));
        }
    }
    #[test]
    fn clipboard_reservation_retains_owner_until_abandoned_work_unwinds() {
        let shared = shared();
        let weak = Arc::downgrade(&shared);
        let held = shared.reserve().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        calls.fetch_add(1, Ordering::Relaxed);
        drop(shared);
        assert!(weak.upgrade().is_some());
        drop(held);
        assert!(weak.upgrade().is_none());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn clipboard_unwind_and_close_failure_are_balanced_and_never_verified() {
        let shared = shared();
        let mut fake = Fake::new();
        fake.panic_data = true;
        assert!(
            catch_unwind(AssertUnwindSafe(|| read_with(
                &mut fake,
                &shared,
                &permit(&shared),
                1,
                ClipKind::Text,
                8
            )))
            .is_err()
        );
        assert_eq!((fake.opens, fake.closes, fake.reads), (1, 1, 1));
        let mut fake = Fake::new();
        fake.close_fails = true;
        assert!(read_with(&mut fake, &shared, &permit(&shared), 1, ClipKind::Text, 8).is_err());
        assert!(shared.native_failed.load(Ordering::Acquire));
        assert!(shared.check().is_err());
    }
    #[test]
    fn clipboard_expired_request_keeps_busy_until_actual_owner_unwinds() {
        let shared = shared();
        let (commands, receiver) = mpsc::sync_channel::<Command>(1);
        let (entered, entry) = mpsc::sync_channel(1);
        let (release, wait) = mpsc::sync_channel(1);
        let owned = shared.clone();
        let owner = thread::spawn(move || {
            let command = receiver.recv().unwrap();
            entered.send(()).unwrap();
            wait.recv().unwrap();
            assert!(command.permit.abandoned.load(Ordering::Acquire));
            let _ = command.reply.send(Err(PlatformError::Timeout));
            drop(command);
            owned.cleaned.store(true, Ordering::Release);
        });
        let backend = WindowsClipboard {
            shared: shared.clone(),
            commands,
            owner: Some(owner),
            delivery: None,
        };
        assert!(matches!(
            backend.request_bounded(
                Operation::Read(ClipKind::Text, 8),
                Duration::from_millis(20)
            ),
            Err(PlatformError::Timeout)
        ));
        entry.recv().unwrap();
        assert!(matches!(backend.kinds(), Err(PlatformError::Timeout)));
        assert!(matches!(
            backend.request(Operation::Read(ClipKind::Text, 8)),
            Err(PlatformError::Timeout)
        ));
        release.send(()).unwrap();
        assert!(WindowsClipboard::stop_verified(backend));
    }
    fn metadata(sequence: u32, owner: usize) -> Snapshot {
        Snapshot {
            sequence,
            owner,
            kinds: ClipKinds {
                text: true,
                image: false,
            },
        }
    }
    #[test]
    fn clipboard_delivery_reopen_reconciles_discarded_event_exactly_once() {
        let shared = shared();
        shared.subscribe(Arc::new(|_| {})).unwrap();
        shared
            .queue(metadata(1, 2), 1, shared.gate.epoch())
            .unwrap();
        shared.gate.set_engine_permits(false);
        shared.gate.set_engine_permits(true);
        assert!(shared.next(&mut shared.delivery.lock().unwrap()).is_none());
        assert!(shared.dirty.load(Ordering::Acquire));
        shared
            .queue(metadata(1, 2), 1, shared.gate.epoch())
            .unwrap();
        assert!(shared.next(&mut shared.delivery.lock().unwrap()).is_some());
        shared
            .queue(metadata(1, 2), 1, shared.gate.epoch())
            .unwrap();
        assert!(shared.next(&mut shared.delivery.lock().unwrap()).is_none());
    }
    #[test]
    fn clipboard_delivery_own_echo_and_sink_replacement_use_current_metadata_only() {
        let shared = shared();
        let old = Arc::new(AtomicUsize::new(0));
        let count = old.clone();
        shared
            .subscribe(Arc::new(move |_| {
                count.fetch_add(1, Ordering::Relaxed);
            }))
            .unwrap();
        shared
            .queue(metadata(1, 2), 1, shared.gate.epoch())
            .unwrap();
        let new = Arc::new(AtomicUsize::new(0));
        let count = new.clone();
        shared
            .subscribe(Arc::new(move |_| {
                count.fetch_add(1, Ordering::Relaxed);
            }))
            .unwrap();
        assert!(shared.next(&mut shared.delivery.lock().unwrap()).is_none());
        shared
            .queue(metadata(2, 1), 1, shared.gate.epoch())
            .unwrap();
        assert!(shared.next(&mut shared.delivery.lock().unwrap()).is_none());
        shared
            .queue(metadata(3, 2), 1, shared.gate.epoch())
            .unwrap();
        let (sink, kinds) = shared.next(&mut shared.delivery.lock().unwrap()).unwrap();
        sink.send(ClipboardEvent::Changed { kinds });
        assert_eq!(old.load(Ordering::Relaxed), 0);
        assert_eq!(new.load(Ordering::Relaxed), 1);
        shared.stop();
        assert!(
            shared
                .queue(metadata(4, 2), 1, shared.gate.epoch())
                .is_err()
        );
    }
    #[test]
    fn clipboard_delivery_callback_can_reenter_without_holding_owner_or_sink_mutex() {
        let shared = shared();
        let weak = Arc::downgrade(&shared);
        let (done, received) = mpsc::sync_channel(1);
        shared
            .subscribe(Arc::new(move |_| {
                let shared = weak.upgrade().unwrap();
                let ticket = shared.reserve().unwrap();
                drop(ticket);
                shared.stop();
                done.send(()).unwrap();
            }))
            .unwrap();
        shared
            .queue(metadata(1, 2), 1, shared.gate.epoch())
            .unwrap();
        let owned = shared.clone();
        let worker = thread::spawn(move || owned.deliver());
        received.recv_timeout(Duration::from_secs(1)).unwrap();
        worker.join().unwrap();
        let weak = Arc::downgrade(&shared);
        drop(shared);
        assert!(weak.upgrade().is_none());
    }
}
