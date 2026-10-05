//! One message-window owner performs every native data call and delayed-format submission. Win32 delayed
//! rendering may hold OpenClipboard for up to 30 seconds; the caller's two-second timeout cannot
//! shorten that OS wait. A timed-out command retains its reservation and RAII resources until
//! the owner returns; later read/kinds calls refuse immediately rather than accumulating work.
//! WM_RENDERFORMAT never opens the clipboard: the paster already owns its open lock.
//! Fulfilment/cancellation use the same shared render ledger, outside the owner command queue.
#![allow(unsafe_code)]

use crate::model::clipboard::{
    self as model, Format, Formats, Render, RenderLedger, Snapshot, Watch,
};
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
    Foundation::{
        GetLastError, GlobalFree, HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, SetLastError, WPARAM,
    },
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
    paste: Option<Render>,
    lost: Option<u64>,
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
    renders: Mutex<RenderLedger>,
    render_wake: Condvar,
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
                paste: None,
                lost: None,
                sink: None,
                watch: Watch::default(),
            }),
            wake: Condvar::new(),
            cleaned: AtomicBool::new(false),
            native_failed: AtomicBool::new(false),
            renders: Mutex::new(RenderLedger::default()),
            render_wake: Condvar::new(),
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
        if let Ok(mut renders) = self.renders.lock() {
            renders.close();
        }
        self.render_wake.notify_all();
        if let Ok(mut delivery) = self.delivery.lock() {
            delivery.pending = None;
            delivery.paste = None;
            delivery.lost = None;
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
        delivery.paste = None;
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
                while delivery.pending.is_none()
                    && delivery.paste.is_none()
                    && delivery.lost.is_none()
                    && !self.stopped.load(Ordering::Acquire)
                {
                    delivery = self.wake.wait(delivery).map_err(|_| ())?;
                }
                let sink = delivery.sink.clone();
                let event = if let Some(offer) = delivery.lost.take() {
                    Some(ClipboardEvent::PromiseLost { offer })
                } else if let Some(render) = delivery.paste.take() {
                    Some(ClipboardEvent::PasteRequested {
                        paste: render.paste,
                        offer: render.offer,
                        kind: if render.format == Format::Text {
                            ClipKind::Text
                        } else {
                            ClipKind::Image
                        },
                    })
                } else {
                    self.next(&mut delivery)
                        .map(|(_, kinds)| ClipboardEvent::Changed { kinds })
                };
                Ok::<_, ()>(sink.zip(event))
            })();
            match next {
                Ok(Some((sink, event))) => {
                    if let ClipboardEvent::PasteRequested { paste, .. } = &event {
                        let valid = self.renders.lock().is_ok_and(|renders| {
                            renders.pending(*paste).is_some_and(|r| {
                                renders.valid(
                                    r,
                                    Instant::now(),
                                    self.gate.epoch(),
                                    self.gate.is_open(),
                                )
                            })
                        });
                        if !valid {
                            continue;
                        }
                    }
                    if catch_unwind(AssertUnwindSafe(|| sink.send(event))).is_err() {
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
    fn reserve_data(self: &Arc<Self>) -> Result<Reservation, PlatformError> {
        // Hold only this short decision lock through reservation. No native operation or sink.
        let renders = self
            .renders
            .lock()
            .map_err(|_| backend("clipboard rendering poisoned"))?;
        if renders.rendering() {
            return Err(PlatformError::Timeout);
        }
        self.reserve()
    }
    fn cancel_render(&self, offer: Option<u64>) {
        if let Ok(mut renders) = self.renders.lock() {
            renders.cancel(offer);
        }
        self.render_wake.notify_all();
    }
    fn revoke(&self, offer: u64) -> Result<bool, PlatformError> {
        let revoked = self
            .renders
            .lock()
            .map_err(|_| backend("clipboard rendering poisoned"))?
            .withdraw(offer);
        self.render_wake.notify_all();
        Ok(revoked)
    }
    fn ownership(&self, ours: bool) -> Result<(), PlatformError> {
        let lost = self
            .renders
            .lock()
            .map_err(|_| backend("clipboard rendering poisoned"))?
            .lost(ours);
        self.render_wake.notify_all();
        if let Some(offer) = lost {
            let mut delivery = self
                .delivery
                .lock()
                .map_err(|_| backend("clipboard delivery poisoned"))?;
            if delivery.lost.is_some() {
                return Err(backend("clipboard loss delivery occupied"));
            }
            delivery.lost = Some(offer);
            self.wake.notify_one();
        }
        Ok(())
    }
    fn begin_render(&self, format: Format) -> Result<Option<Render>, PlatformError> {
        self.check()?;
        let render = self
            .renders
            .lock()
            .map_err(|_| backend("clipboard rendering poisoned"))?
            .begin(
                format,
                Instant::now(),
                self.gate.epoch(),
                self.gate.is_open(),
            );
        if let Some(render) = render {
            let mut delivery = self
                .delivery
                .lock()
                .map_err(|_| backend("clipboard delivery poisoned"))?;
            if delivery.sink.is_none() {
                drop(delivery);
                self.cancel_render(Some(render.offer));
            } else {
                delivery.paste = Some(render);
                self.wake.notify_one();
            }
        }
        Ok(render)
    }
    fn fulfil(&self, paste: LocalPasteId, data: Option<Vec<u8>>) {
        let data = data.map(Zeroizing::new);
        let render = self
            .renders
            .lock()
            .ok()
            .and_then(|renders| renders.pending(paste));
        let Some(render) = render else {
            return;
        };
        // Conversion is outside all owner/delivery/ledger locks; a synchronous sink may fulfil.
        let native = data
            .and_then(|data| {
                match render.format {
                    Format::Text => model::text_native(&data),
                    Format::Png => model::png(&data, model::IMAGE_CAP),
                    Format::DibV5 => model::png_dibv5(&data),
                    Format::Dib => Err(PlatformError::NotFound),
                }
                .ok()
            })
            .map(Zeroizing::new);
        if let Ok(mut renders) = self.renders.lock() {
            renders.answer(
                paste,
                native,
                Instant::now(),
                self.gate.epoch(),
                self.gate.is_open(),
            );
        }
        self.render_wake.notify_all();
    }
}
trait RenderAccess {
    fn ours(&self) -> bool;
    fn write(&mut self, render: Render, data: &[u8]) -> Result<(), PlatformError>;
}
fn render_with(
    _shared: &Shared,
    _format: Format,
    _port: &mut impl RenderAccess,
) -> Result<(), PlatformError> {
    if !_port.ours() {
        return Ok(());
    }
    let Some(render) = _shared.begin_render(_format)? else {
        return Ok(());
    };
    struct Finish<'a>(&'a Shared, LocalPasteId);
    impl Drop for Finish<'_> {
        fn drop(&mut self) {
            if let Ok(mut renders) = self.0.renders.lock() {
                renders.finish(self.1);
            }
        }
    }
    let _finish = Finish(_shared, render.paste);
    let mut renders = _shared
        .renders
        .lock()
        .map_err(|_| backend("clipboard rendering poisoned"))?;
    let data = loop {
        if let Some(data) = renders.poll(
            render,
            Instant::now(),
            _shared.gate.epoch(),
            _shared.gate.is_open(),
        ) {
            break data;
        }
        let wait = POLL.min(render.deadline.saturating_duration_since(Instant::now()));
        renders = _shared
            .render_wake
            .wait_timeout(renders, wait)
            .map_err(|_| backend("clipboard rendering poisoned"))?
            .0;
    };
    let allowed = renders.valid(
        render,
        Instant::now(),
        _shared.gate.epoch(),
        _shared.gate.is_open(),
    );
    drop(renders);
    if let Some(data) = data.filter(|_| allowed && _port.ours()) {
        _port.write(render, &data)?;
    }
    Ok(())
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
    fn snapshot(&mut self) -> Result<(Snapshot, Formats), PlatformError> {
        Err(backend("clipboard control metadata unavailable"))
    }
    fn data(
        &mut self,
        _format: Format,
        _limit: usize,
    ) -> Result<Zeroizing<Vec<u8>>, PlatformError> {
        Err(backend("clipboard control cannot read data"))
    }
}
trait ControlAccess: Access {
    fn ours(&mut self) -> bool;
    fn admit(&mut self) -> Result<(), PlatformError>;
    fn empty(&mut self) -> Result<(), PlatformError>;
    fn advertise(&mut self, format: Format) -> Result<(), PlatformError>;
    fn recorded(&mut self);
}
fn promise_with(
    port: &mut impl ControlAccess,
    shared: &Shared,
    permit: &Permit,
    offer: u64,
    kinds: ClipKinds,
) -> Result<(), PlatformError> {
    permit.check(shared)?;
    RenderLedger::validate(kinds, shared.gate.is_open())?;
    if shared
        .delivery
        .lock()
        .map_err(|_| backend("clipboard delivery poisoned"))?
        .lost
        .is_some()
    {
        return Err(PlatformError::Timeout);
    }
    let mut opened = open_control(port, shared, Some(permit))?;
    opened.port.admit()?;
    permit.check(shared)?;
    opened.port.empty()?;
    // Preserve the exact admitted post-mutation sequence even when a later step fails.
    opened.port.recorded();
    let promise = shared
        .renders
        .lock()
        .map_err(|_| backend("clipboard rendering poisoned"))?
        .install(offer, kinds, permit.epoch, shared.gate.is_open())?;
    let submitted = (|| {
        for format in [
            kinds.text.then_some(Format::Text),
            kinds.image.then_some(Format::Png),
            kinds.image.then_some(Format::DibV5),
        ]
        .into_iter()
        .flatten()
        {
            permit.check(shared)?;
            opened.port.advertise(format)?;
            opened.port.recorded();
        }
        permit.check(shared)
    })();
    if submitted.is_err() {
        shared.revoke(promise.offer)?;
    }
    let closed = opened.close();
    submitted?;
    closed?;
    permit.check(shared)
}
fn withdraw_with(
    port: &mut impl ControlAccess,
    shared: &Shared,
    permit: Option<&Permit>,
    offer: u64,
) -> Result<(), PlatformError> {
    let owned = shared
        .renders
        .lock()
        .map_err(|_| backend("clipboard rendering poisoned"))?
        .owned()
        .filter(|p| p.offer == offer);
    let Some(owned) = owned else {
        return Ok(());
    };
    if !port.ours() {
        shared.ownership(false)?;
        return Ok(());
    }
    let mut opened = open_control(port, shared, permit)?;
    if opened.port.ours() {
        opened.port.admit()?;
        if permit
            .is_some_and(|p| p.abandoned.load(Ordering::Acquire) || Instant::now() >= p.deadline)
        {
            return Err(PlatformError::Timeout);
        }
        opened.port.empty()?;
        opened.port.recorded();
        shared
            .renders
            .lock()
            .map_err(|_| backend("clipboard rendering poisoned"))?
            .forget(owned);
    } else {
        shared.ownership(false)?;
    }
    opened.close()
}
fn render_all_with(port: &mut impl ControlAccess, shared: &Shared) -> Result<(), PlatformError> {
    shared.cancel_render(None);
    let mut opened = open_control(port, shared, None)?;
    // GetClipboardOwner must be rechecked after OpenClipboard. Neither case fetches or writes.
    let _still_ours = opened.port.ours();
    opened.close()
}
fn destroy_notice(shared: &Shared, intentional: bool) -> Result<(), PlatformError> {
    if intentional {
        Ok(())
    } else {
        shared.ownership(false)
    }
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
        let closed = self.port.close();
        if closed.is_err() {
            self.shared.native_failed.store(true, Ordering::Release);
            self.shared.stop();
        }
        closed
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
    Promise(u64, ClipKinds),
    Withdraw(u64),
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

/// One clipboard owner, one outstanding delayed render and an out-of-band fulfil/cancel path.
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
        let reservation = if matches!(operation, Operation::Kinds | Operation::Read(..)) {
            self.shared.reserve_data()?
        } else {
            self.shared.reserve()?
        };
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
    #[cfg(test)]
    #[allow(dead_code)] // Used by the source-included Limited probe, absent in the lib-test root.
    pub(crate) fn probe_fulfiller(
        &self,
    ) -> impl Fn(LocalPasteId, Option<Vec<u8>>) + Send + Sync + 'static {
        let weak = Arc::downgrade(&self.shared);
        move |paste, data| {
            if let Some(shared) = weak.upgrade() {
                shared.fulfil(paste, data);
            }
        }
    }
}
fn backend_error_spawn() -> PlatformError {
    backend("spawn clipboard worker failed")
}
impl ClipboardHost for WindowsClipboard {
    fn subscribe(&mut self, sink: Sink) -> Result<(), PlatformError> {
        self.shared.cancel_render(None);
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
    fn promise(&mut self, offer: u64, kinds: ClipKinds) -> Result<(), PlatformError> {
        RenderLedger::validate(kinds, self.shared.gate.is_open())?;
        // Wake a blocked owner before the bounded command handoff. No mutex spans native I/O.
        let owned = self
            .shared
            .renders
            .lock()
            .map_err(|_| backend("clipboard rendering poisoned"))?
            .owned();
        if let Some(owned) = owned {
            self.shared.revoke(owned.offer)?;
        }
        match self.request(Operation::Promise(offer, kinds))? {
            Reply::Unit => Ok(()),
            _ => Err(backend("clipboard reply type mismatch")),
        }
    }
    fn fulfil(&mut self, paste: LocalPasteId, data: Option<Vec<u8>>) {
        self.shared.fulfil(paste, data);
    }
    fn withdraw(&mut self, offer: u64) -> Result<(), PlatformError> {
        if !self.shared.revoke(offer)? {
            return Ok(());
        }
        match self.request(Operation::Withdraw(offer))? {
            Reply::Unit => Ok(()),
            _ => Err(backend("clipboard reply type mismatch")),
        }
    }
}
impl Drop for WindowsClipboard {
    fn drop(&mut self) {
        if !self.finish() {
            eprintln!("Windows clipboard cleanup unverified: owner or delivery retained");
        }
    }
}

thread_local! {
    static SIGNAL: Cell<*const Shared> = const { Cell::new(null()) };
    static PNG_FORMAT: Cell<u32> = const { Cell::new(0) };
    static OWN_EMPTY: Cell<bool> = const { Cell::new(false) };
}
unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    let handled = SIGNAL.with(|signal| {
        let pointer = signal.get();
        if pointer.is_null() {
            return false;
        }
        // SAFETY: stable Arc-backed context installed before CreateWindow; retained until after
        // DestroyWindow. No Native borrow spans callbacks or reentrant Empty/SetClipboardData.
        let shared = unsafe { &*pointer };
        if message == WM_CLIPBOARDUPDATE {
            shared.dirty.store(true, Ordering::Release);
            return true;
        }
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<bool, PlatformError> {
            match message {
                WM_RENDERFORMAT => {
                    let format = PNG_FORMAT.with(|png| {
                        if wp as u32 == png.get() {
                            Some(Format::Png)
                        } else {
                            match wp as u32 {
                                n if n == u32::from(CF_UNICODETEXT) => Some(Format::Text),
                                n if n == u32::from(CF_DIBV5) => Some(Format::DibV5),
                                _ => None,
                            }
                        }
                    });
                    if let Some(format) = format {
                        render_with(shared, format, &mut WinRender { window, shared })?;
                    }
                    Ok(true)
                }
                WM_RENDERALLFORMATS => {
                    shared.cancel_render(None);
                    render_all(window, shared)?;
                    Ok(true)
                }
                WM_DESTROYCLIPBOARD => {
                    destroy_notice(shared, OWN_EMPTY.with(Cell::get))?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        }));
        match result {
            Ok(Ok(handled)) => handled,
            _ => {
                shared.native_failed.store(true, Ordering::Release);
                shared.stop();
                eprintln!("Windows clipboard render callback failed");
                true
            }
        }
    });
    if handled {
        return 0;
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
        SIGNAL.with(|signal| signal.set(Arc::as_ptr(&native.shared)));
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
        PNG_FORMAT.with(|png| png.set(native.png));
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
        self.shared
            .ownership(snapshot.owner == self.window as usize)?;
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
            Operation::Promise(offer, kinds) => {
                self.promise(*offer, *kinds, &command.permit)?;
                Ok(Reply::Unit)
            }
            Operation::Withdraw(offer) => {
                self.withdraw(*offer, Some(&command.permit))?;
                Ok(Reply::Unit)
            }
        }
    }
    fn control(&self) -> WinControl<'_> {
        WinControl {
            window: self.window,
            png: self.png,
            shared: &self.shared,
        }
    }
    fn promise(&self, offer: u64, kinds: ClipKinds, permit: &Permit) -> Result<(), PlatformError> {
        promise_with(&mut self.control(), &self.shared, permit, offer, kinds)
    }
    fn withdraw(&self, offer: u64, permit: Option<&Permit>) -> Result<(), PlatformError> {
        withdraw_with(&mut self.control(), &self.shared, permit, offer)
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
        let owned = self
            .shared
            .renders
            .lock()
            .ok()
            .and_then(|renders| renders.owned());
        if let Some(owned) = owned {
            cleaned &= self.withdraw(owned.offer, None).is_ok();
        }
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
        PNG_FORMAT.with(|png| png.set(0));
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
fn open_control<'a, T: Access>(
    port: &'a mut T,
    shared: &'a Shared,
    permit: Option<&Permit>,
) -> Result<OpenGuard<'a, T>, PlatformError> {
    let started = Instant::now();
    loop {
        if permit
            .is_some_and(|p| p.abandoned.load(Ordering::Acquire) || Instant::now() >= p.deadline)
        {
            return Err(PlatformError::Timeout);
        }
        let delay = model::retry_delay(started.elapsed().as_millis() as u64)?;
        if port.open()? {
            return Ok(OpenGuard {
                port,
                active: true,
                shared,
            });
        }
        thread::sleep(Duration::from_millis(delay));
    }
}
fn empty_owned() -> Result<(), PlatformError> {
    struct Intent(bool);
    impl Drop for Intent {
        fn drop(&mut self) {
            OWN_EMPTY.with(|intent| intent.set(self.0));
        }
    }
    let _intent = Intent(OWN_EMPTY.with(|intent| intent.replace(true)));
    // SAFETY: caller holds OpenClipboard, has checked mutation admission, and keeps reentrant
    // own-empty notification distinct from foreign ownership loss without holding a ledger lock.
    if unsafe { EmptyClipboard() } == 0 {
        Err(os_error("empty own clipboard"))
    } else {
        Ok(())
    }
}
fn render_all(window: HWND, shared: &Shared) -> Result<(), PlatformError> {
    render_all_with(
        &mut WinControl {
            window,
            png: 0,
            shared,
        },
        shared,
    )
}
struct WinControl<'a> {
    window: HWND,
    png: u32,
    shared: &'a Shared,
}
impl Access for WinControl<'_> {
    fn open(&mut self) -> Result<bool, PlatformError> {
        // SAFETY: own observer HWND; successful open has an owner-thread RAII close.
        Ok(unsafe { OpenClipboard(self.window) } != 0)
    }
    fn close(&mut self) -> Result<(), PlatformError> {
        // SAFETY: balances exactly this control port's open, on its owning thread.
        if unsafe { CloseClipboard() } == 0 {
            Err(os_error("close clipboard control"))
        } else {
            Ok(())
        }
    }
}
impl ControlAccess for WinControl<'_> {
    fn ours(&mut self) -> bool {
        // SAFETY: scalar metadata only, no foreign HWND fields or content.
        unsafe { GetClipboardOwner() == self.window }
    }
    fn admit(&mut self) -> Result<(), PlatformError> {
        #[cfg(test)]
        probe_admit_mutation()?;
        Ok(())
    }
    fn empty(&mut self) -> Result<(), PlatformError> {
        empty_owned()
    }
    fn advertise(&mut self, format: Format) -> Result<(), PlatformError> {
        let id = match format {
            Format::Text => u32::from(CF_UNICODETEXT),
            Format::Png => self.png,
            Format::DibV5 => u32::from(CF_DIBV5),
            Format::Dib => return Err(PlatformError::NotFound),
        };
        if id == 0 {
            return Err(backend("clipboard promise format unavailable"));
        }
        // SAFETY: our open and newly acquired clipboard ownership; NULL advertises delayed data.
        // NULL return is ambiguous, so distinguish error and verify actual advertisement.
        unsafe {
            SetLastError(0);
            SetClipboardData(id, null_mut());
            if GetLastError() != 0 || IsClipboardFormatAvailable(id) == 0 {
                return Err(os_error("promise clipboard format"));
            }
        }
        Ok(())
    }
    fn recorded(&mut self) {
        #[cfg(test)]
        update_probe_owned(self.window as usize);
        let _ = self.shared;
    }
}
struct WinRender<'a> {
    window: HWND,
    shared: &'a Shared,
}
impl RenderAccess for WinRender<'_> {
    fn ours(&self) -> bool {
        // SAFETY: owner metadata only, no foreign fields or OpenClipboard in render callback.
        unsafe { GetClipboardOwner() == self.window }
    }
    fn write(&mut self, render: Render, data: &[u8]) -> Result<(), PlatformError> {
        let format = match render.format {
            Format::Text => u32::from(CF_UNICODETEXT),
            Format::Png => PNG_FORMAT.with(Cell::get),
            Format::DibV5 => u32::from(CF_DIBV5),
            Format::Dib => return Ok(()),
        };
        if format == 0 {
            return Err(backend("clipboard render format unavailable"));
        }
        let mut memory = OwnedMemory::new(data, self.shared)?;
        let allowed = self
            .shared
            .renders
            .lock()
            .map_err(|_| backend("clipboard rendering poisoned"))?
            .valid(
                render,
                Instant::now(),
                self.shared.gate.epoch(),
                self.shared.gate.is_open(),
            );
        if !allowed || !self.ours() {
            return Ok(());
        }
        #[cfg(test)]
        probe_admit_mutation()?;
        // SAFETY: WM_RENDERFORMAT owns no OpenClipboard; the requester keeps it open. Exactly
        // this promised format receives our GMEM_MOVEABLE handle. On success the OS owns it.
        if unsafe { SetClipboardData(format, memory.handle) }.is_null() {
            return Err(os_error("render clipboard data"));
        }
        memory.handle = null_mut();
        #[cfg(test)]
        update_probe_owned(self.window as usize);
        Ok(())
    }
}
struct OwnedMemory<'a> {
    handle: HGLOBAL,
    size: usize,
    shared: &'a Shared,
}
impl<'a> OwnedMemory<'a> {
    fn new(data: &[u8], shared: &'a Shared) -> Result<Self, PlatformError> {
        // SAFETY: bounded pure converter output determines exact movable allocation size.
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, data.len()) };
        if handle.is_null() {
            return Err(os_error("allocate rendered clipboard memory"));
        }
        let memory = Self {
            handle,
            size: data.len(),
            shared,
        };
        // SAFETY: own valid allocation, initialized in full before any clipboard transfer.
        let pointer = unsafe { GlobalLock(handle) };
        if pointer.is_null() {
            return Err(os_error("lock rendered clipboard memory"));
        }
        // SAFETY: allocation is at least data.len(), source is live/nonoverlapping, and balanced
        // unlock precedes transfer. It is not a borrowed clipboard HGLOBAL.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), pointer.cast::<u8>(), data.len());
            SetLastError(0);
            if GlobalUnlock(handle) == 0 && GetLastError() != 0 {
                return Err(os_error("unlock rendered clipboard memory"));
            }
        }
        Ok(memory)
    }
}
impl Drop for OwnedMemory<'_> {
    fn drop(&mut self) {
        if self.handle.is_null() {
            return;
        }
        // SAFETY: this untransferred allocation still belongs exclusively to us. Zero our exact
        // initialized range; allocator padding was zero-initialized at allocation time.
        unsafe {
            let pointer = GlobalLock(self.handle);
            if pointer.is_null() {
                self.shared.native_failed.store(true, Ordering::Release);
            } else {
                std::ptr::write_bytes(pointer.cast::<u8>(), 0, self.size);
                SetLastError(0);
                if GlobalUnlock(self.handle) == 0 && GetLastError() != 0 {
                    self.shared.native_failed.store(true, Ordering::Release);
                }
            }
        }
        // SAFETY: only an untransferred allocation is freed. OS-owned handles are cleared after
        // successful SetClipboardData and never accessed again. No foreign resource cleanup.
        if !unsafe { GlobalFree(self.handle) }.is_null() {
            self.shared.native_failed.store(true, Ordering::Release);
            eprintln!("Windows clipboard rendered allocation cleanup unverified");
        }
    }
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
#[allow(dead_code)] // Source-included probe admission only; no native calls in ordinary cargo tests.
pub(crate) fn probe_fixture() -> Option<(usize, u32)> {
    PROBE_FIXTURE.lock().ok().and_then(|owner| *owner)
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
fn probe_admit_mutation() -> Result<(), PlatformError> {
    let expected = *PROBE_FIXTURE
        .lock()
        .map_err(|_| backend("probe admission poisoned"))?;
    // SAFETY: caller already holds clipboard open, or is its WM_RENDERFORMAT owner callback
    // while the authenticated fixture paster holds it. Only metadata is read before mutation.
    let admitted = unsafe {
        let sequence = GetClipboardSequenceNumber();
        if let Some((owner, expected)) = expected {
            let mut pid = 0;
            sequence != 0
                && sequence == expected
                && GetClipboardOwner() as usize == owner
                && IsWindow(owner as HWND) != 0
                && GetWindowThreadProcessId(owner as HWND, &mut pid) != 0
                && pid == std::process::id()
        } else {
            SetLastError(0);
            let formats = CountClipboardFormats();
            sequence != 0
                && formats == 0
                && GetLastError() == 0
                && sequence == GetClipboardSequenceNumber()
        }
    };
    if admitted {
        Ok(())
    } else {
        Err(PlatformError::Unsupported(
            "clipboard fixture replaced before mutation",
        ))
    }
}
#[cfg(test)]
fn update_probe_owned(owner: usize) {
    // SAFETY: called after our admitted native operation, before another mutation is allowed.
    let current = unsafe { (GetClipboardOwner() as usize, GetClipboardSequenceNumber()) };
    if current.0 == owner && current.1 != 0 {
        set_probe_fixture(owner, current.1);
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
    struct RenderFake {
        writes: usize,
        ours: bool,
    }
    impl RenderAccess for RenderFake {
        fn ours(&self) -> bool {
            self.ours
        }
        fn write(&mut self, _: Render, data: &[u8]) -> Result<(), PlatformError> {
            assert_eq!(
                model::text(data, model::TEXT_CAP).unwrap(),
                b"fixture\nvalue"
            );
            self.writes += 1;
            Ok(())
        }
    }
    fn promise(shared: &Shared) {
        shared
            .renders
            .lock()
            .unwrap()
            .install(
                41,
                ClipKinds {
                    text: true,
                    image: true,
                },
                shared.gate.epoch(),
                true,
            )
            .unwrap();
    }
    struct ControlFake {
        opens: usize,
        closes: usize,
        empties: usize,
        owner_checks: usize,
        admissions: usize,
        ours: bool,
        replace_on_open: bool,
        admitted: bool,
        fail_empty: bool,
        fail_advertise: bool,
        formats: Vec<Format>,
        reentrant: Arc<Shared>,
    }
    impl ControlFake {
        fn new(shared: &Arc<Shared>) -> Self {
            Self {
                opens: 0,
                closes: 0,
                empties: 0,
                owner_checks: 0,
                admissions: 0,
                ours: true,
                replace_on_open: false,
                admitted: true,
                fail_empty: false,
                fail_advertise: false,
                formats: Vec::new(),
                reentrant: shared.clone(),
            }
        }
    }
    impl Access for ControlFake {
        fn open(&mut self) -> Result<bool, PlatformError> {
            self.opens += 1;
            if self.replace_on_open {
                self.ours = false;
            }
            Ok(true)
        }
        fn close(&mut self) -> Result<(), PlatformError> {
            self.closes += 1;
            Ok(())
        }
        fn data(&mut self, _: Format, _: usize) -> Result<Zeroizing<Vec<u8>>, PlatformError> {
            panic!("control fetched clipboard content")
        }
    }
    impl ControlAccess for ControlFake {
        fn ours(&mut self) -> bool {
            self.owner_checks += 1;
            self.ours
        }
        fn admit(&mut self) -> Result<(), PlatformError> {
            self.admissions += 1;
            if self.admitted {
                Ok(())
            } else {
                Err(PlatformError::Unsupported("fake admission changed"))
            }
        }
        fn empty(&mut self) -> Result<(), PlatformError> {
            self.empties += 1;
            destroy_notice(&self.reentrant, true)?;
            if self.fail_empty {
                Err(backend("fake empty failed"))
            } else {
                Ok(())
            }
        }
        fn advertise(&mut self, format: Format) -> Result<(), PlatformError> {
            self.formats.push(format);
            if self.fail_advertise {
                Err(backend("fake advertisement failed"))
            } else {
                Ok(())
            }
        }
        fn recorded(&mut self) {}
    }
    #[test]
    fn clipboard_promise_advertises_only_requested_formats_and_own_empty_is_not_loss() {
        let shared = shared();
        promise(&shared);
        let mut port = ControlFake::new(&shared);
        promise_with(
            &mut port,
            &shared,
            &permit(&shared),
            42,
            ClipKinds {
                text: true,
                image: true,
            },
        )
        .unwrap();
        assert_eq!(
            (port.opens, port.closes, port.empties, port.admissions),
            (1, 1, 1, 1)
        );
        assert_eq!(port.formats, [Format::Text, Format::Png, Format::DibV5]);
        assert_eq!(shared.renders.lock().unwrap().current().unwrap().offer, 42);
        assert!(shared.delivery.lock().unwrap().lost.is_none());
        assert!(shared.delivery.lock().unwrap().pending.is_none());
    }
    #[test]
    fn clipboard_promise_preflight_and_native_failure_close_without_fetch_or_stale_authority() {
        for mode in 0..4 {
            let shared = shared();
            let mut port = ControlFake::new(&shared);
            let permit = permit(&shared);
            match mode {
                0 => permit.abandoned.store(true, Ordering::Release),
                1 => port.admitted = false,
                2 => port.fail_empty = true,
                _ => port.fail_advertise = true,
            }
            assert!(
                promise_with(
                    &mut port,
                    &shared,
                    &permit,
                    42,
                    ClipKinds {
                        text: true,
                        image: false
                    }
                )
                .is_err()
            );
            assert_eq!(port.closes, port.opens);
            assert!(shared.renders.lock().unwrap().current().is_none());
            if mode == 0 {
                assert_eq!(port.opens, 0);
            }
            if mode == 1 {
                assert_eq!(port.empties, 0);
            }
            permit.abandoned.store(true, Ordering::Release);
        }
    }
    #[test]
    fn clipboard_withdraw_rechecks_owner_after_open_and_never_clears_replacement() {
        for replacement in [false, true] {
            let shared = shared();
            promise(&shared);
            let mut port = ControlFake::new(&shared);
            port.replace_on_open = replacement;
            assert!(shared.revoke(41).unwrap());
            withdraw_with(&mut port, &shared, Some(&permit(&shared)), 41).unwrap();
            assert_eq!((port.opens, port.closes), (1, 1));
            assert!(port.owner_checks >= 2);
            assert_eq!(port.empties, usize::from(!replacement));
            assert!(shared.renders.lock().unwrap().owned().is_none());
            assert!(shared.delivery.lock().unwrap().lost.is_none());
        }
    }
    #[test]
    fn clipboard_withdraw_wrong_offer_or_foreign_owner_is_zero_mutation() {
        let shared = shared();
        promise(&shared);
        let mut port = ControlFake::new(&shared);
        withdraw_with(&mut port, &shared, None, 40).unwrap();
        assert_eq!((port.opens, port.empties), (0, 0));
        port.ours = false;
        withdraw_with(&mut port, &shared, None, 41).unwrap();
        assert_eq!((port.opens, port.empties), (0, 0));
        assert_eq!(shared.delivery.lock().unwrap().lost, Some(41));
    }
    #[test]
    fn clipboard_render_all_no_fetch_or_empty_and_drop_withdraws_only_retained_ours() {
        let shared = shared();
        promise(&shared);
        let render = shared
            .renders
            .lock()
            .unwrap()
            .begin(Format::Text, Instant::now(), shared.gate.epoch(), true)
            .unwrap();
        let mut port = ControlFake::new(&shared);
        render_all_with(&mut port, &shared).unwrap();
        assert_eq!((port.opens, port.closes, port.empties), (1, 1, 0));
        assert!(port.owner_checks > 0);
        assert!(port.formats.is_empty());
        assert!(
            shared
                .renders
                .lock()
                .unwrap()
                .poll(render, Instant::now(), shared.gate.epoch(), true)
                .unwrap()
                .is_none()
        );
        shared.renders.lock().unwrap().finish(render.paste);
        shared.stop();
        withdraw_with(&mut port, &shared, None, 41).unwrap();
        assert_eq!((port.opens, port.closes, port.empties), (2, 2, 1));
        assert!(shared.renders.lock().unwrap().owned().is_none());
        assert!(shared.delivery.lock().unwrap().lost.is_none());
    }
    #[test]
    fn clipboard_render_callback_reentrant_fulfil_never_opens_or_holds_sink_ledger() {
        let shared = shared();
        promise(&shared);
        let weak = Arc::downgrade(&shared);
        let (sent, seen) = mpsc::sync_channel(1);
        shared
            .subscribe(Arc::new(move |event| {
                if let ClipboardEvent::PasteRequested { paste, offer, kind } = event {
                    assert_eq!((offer, kind), (41, ClipKind::Text));
                    let shared = weak.upgrade().unwrap();
                    assert!(matches!(shared.reserve_data(), Err(PlatformError::Timeout)));
                    shared.fulfil(paste, Some(b"fixture\nvalue".to_vec()));
                    sent.send(()).unwrap();
                }
            }))
            .unwrap();
        let worker_shared = shared.clone();
        let worker = thread::spawn(move || worker_shared.deliver());
        let mut port = RenderFake {
            writes: 0,
            ours: true,
        };
        render_with(&shared, Format::Text, &mut port).unwrap();
        assert_eq!(port.writes, 1);
        seen.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(!shared.renders.lock().unwrap().rendering());
        shared.stop();
        worker.join().unwrap();
    }
    #[test]
    fn clipboard_render_unanswered_wait_expires_total_budget_and_releases_slot() {
        let shared = shared();
        promise(&shared);
        shared.subscribe(Arc::new(|_| {})).unwrap();
        let owned = shared.clone();
        let worker = thread::spawn(move || owned.deliver());
        let mut port = RenderFake {
            writes: 0,
            ours: true,
        };
        let start = Instant::now();
        render_with(&shared, Format::Text, &mut port).unwrap();
        assert_eq!(port.writes, 0);
        assert!(start.elapsed() >= model::RENDER_WAIT);
        assert!(!shared.renders.lock().unwrap().rendering());
        shared.stop();
        worker.join().unwrap();
    }
    #[test]
    fn clipboard_public_promise_and_withdraw_cancel_before_bounded_owner_handoff() {
        for replace in [false, true] {
            let shared = shared();
            promise(&shared);
            let render = shared
                .renders
                .lock()
                .unwrap()
                .begin(Format::Text, Instant::now(), shared.gate.epoch(), true)
                .unwrap();
            let (commands, receiver) = mpsc::sync_channel::<Command>(1);
            let owned = shared.clone();
            let worker = thread::spawn(move || {
                let command = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
                assert!(
                    owned
                        .renders
                        .lock()
                        .unwrap()
                        .poll(render, Instant::now(), owned.gate.epoch(), true)
                        .unwrap()
                        .is_none()
                );
                assert!(owned.renders.lock().unwrap().current().is_none());
                assert!(matches!(
                    (&command.operation, replace),
                    (Operation::Promise(42, _), true) | (Operation::Withdraw(41), false)
                ));
                command.reply.send(Ok(Reply::Unit)).unwrap();
                drop(command);
                owned.cleaned.store(true, Ordering::Release);
            });
            let mut backend = WindowsClipboard {
                shared,
                commands,
                owner: Some(worker),
                delivery: None,
            };
            if replace {
                backend
                    .promise(
                        42,
                        ClipKinds {
                            text: true,
                            image: false,
                        },
                    )
                    .unwrap();
            } else {
                backend.withdraw(41).unwrap();
            }
            assert!(backend.stop_verified());
        }
    }
    #[test]
    fn clipboard_render_none_withdraw_drop_and_gate_reopen_are_empty_and_bounded() {
        for mode in 0..4 {
            let shared = shared();
            promise(&shared);
            let weak = Arc::downgrade(&shared);
            let seen = Arc::new(AtomicUsize::new(0));
            let count = seen.clone();
            shared
                .subscribe(Arc::new(move |event| {
                    if let ClipboardEvent::PasteRequested { paste, .. } = event {
                        count.fetch_add(1, Ordering::Relaxed);
                        let shared = weak.upgrade().unwrap();
                        match mode {
                            0 => shared.fulfil(paste, None),
                            1 => {
                                assert!(shared.revoke(41).unwrap());
                            }
                            2 => shared.stop(),
                            _ => {
                                shared.gate.set_session_permits(false);
                                shared.gate.set_session_permits(true);
                                shared.fulfil(paste, Some(vec![1]));
                            }
                        }
                    }
                }))
                .unwrap();
            let worker_shared = shared.clone();
            let worker = thread::spawn(move || worker_shared.deliver());
            let mut port = RenderFake {
                writes: 0,
                ours: true,
            };
            let start = Instant::now();
            render_with(&shared, Format::Text, &mut port).unwrap();
            assert_eq!(port.writes, 0);
            assert!(!shared.renders.lock().unwrap().rendering());
            assert_eq!(seen.load(Ordering::Relaxed), 1);
            assert!(start.elapsed() < Duration::from_secs(1));
            shared.stop();
            worker.join().unwrap();
        }
    }
    #[test]
    fn clipboard_render_unknown_format_foreign_owner_and_second_slot_emit_nothing() {
        let shared = shared();
        promise(&shared);
        shared
            .subscribe(Arc::new(|_| panic!("unexpected fetch")))
            .unwrap();
        let mut port = RenderFake {
            writes: 0,
            ours: false,
        };
        render_with(&shared, Format::Text, &mut port).unwrap();
        assert!(shared.delivery.lock().unwrap().paste.is_none());
        port.ours = true;
        render_with(&shared, Format::Dib, &mut port).unwrap();
        assert!(shared.delivery.lock().unwrap().paste.is_none());
        let first = shared
            .renders
            .lock()
            .unwrap()
            .begin(Format::Text, Instant::now(), shared.gate.epoch(), true)
            .unwrap();
        render_with(&shared, Format::Text, &mut port).unwrap();
        assert!(shared.delivery.lock().unwrap().paste.is_none());
        shared.renders.lock().unwrap().finish(first.paste);
        assert_eq!(port.writes, 0);
    }
    #[test]
    fn clipboard_render_ownership_loss_is_once_without_changed_or_foreign_clear() {
        let shared = shared();
        promise(&shared);
        shared.subscribe(Arc::new(|_| {})).unwrap();
        shared.ownership(true).unwrap();
        assert!(shared.delivery.lock().unwrap().lost.is_none());
        shared.ownership(false).unwrap();
        shared.ownership(false).unwrap();
        assert_eq!(shared.delivery.lock().unwrap().lost, Some(41));
        assert!(shared.renders.lock().unwrap().current().is_none());
        assert!(shared.delivery.lock().unwrap().pending.is_none());
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
