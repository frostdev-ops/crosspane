//! Main-thread-local native lifetimes; dispatch closures carry only synchronized Rust state.
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::AtomicIsize;
use std::sync::mpsc;

use block2::RcBlock;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, MainThreadMarker, define_class, msg_send};
use objc2_app_kit::{NSPasteboardItem, NSPasteboardItemDataProvider, NSPasteboardWriting};
use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
use objc2_foundation::{
    NSArray, NSData, NSDate, NSDefaultRunLoopMode, NSObject, NSObjectProtocol, NSRunLoop,
};

use super::*;

// Unlike main dispatch, default-mode blocks can run inside a provider's nested wait.
pub(super) fn on_default<R: Send + 'static>(
    f: impl FnOnce(MainThreadMarker) -> R + Send + 'static,
) -> Result<R, PlatformError> {
    if MainThreadMarker::new().is_some() {
        return on_main(MAIN_WAIT, f);
    }
    let main = CFRunLoop::main().ok_or_else(poisoned)?;
    let (tx, rx) = mpsc::sync_channel(1);
    let task = Mutex::new(Some(f));
    let block = RcBlock::new(move || {
        let task = task.lock().ok().and_then(|mut task| task.take());
        if let Some(task) = task {
            let _ = tx.send(on_main(MAIN_WAIT, task));
        }
    });
    // SAFETY: Public main run loop/default mode; CF copies the block. Only Send Rust
    // state crosses threads, and all AppKit work still passes through on_main.
    unsafe {
        let mode = kCFRunLoopDefaultMode.ok_or_else(poisoned)?;
        main.perform_block(Some(mode.as_ref()), Some(&block));
    }
    main.wake_up();
    rx.recv_timeout(MAIN_WAIT)
        .map_err(|_| PlatformError::Timeout)?
}

type Native = (Retained<NSPasteboardItem>, Retained<Provider>);
thread_local! {
    static NATIVE: RefCell<HashMap<(String, isize), Native>> = RefCell::new(HashMap::new());
    static WAITING: Cell<bool> = const { Cell::new(false) };
}
struct Wait;
impl Drop for Wait {
    fn drop(&mut self) {
        WAITING.set(false);
    }
}

pub(super) struct Context {
    gate: Arc<IoGate>,
    shared: Arc<Shared>,
    name: PasteboardName,
    marker: String,
    offer: u64,
    kinds: ClipKinds,
    written: AtomicIsize,
    finished: AtomicBool,
    pub(super) abandoned: AtomicBool,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements. Ivars contain synchronized Rust state;
    // callbacks marshal all native work through on_main, without moving retained objects.
    #[unsafe(super = NSObject)]
    #[name = "CrosspaneClipboardProvider"]
    #[thread_kind = AnyThread]
    #[ivars = Arc<Context>]
    struct Provider;
    // SAFETY: NSObjectProtocol has no additional requirements.
    unsafe impl NSObjectProtocol for Provider {}
    // SAFETY: These are the exact public protocol signatures; the native cache retains both
    // the provider and item until the finished notification, including during reentrant calls.
    unsafe impl NSPasteboardItemDataProvider for Provider {
        // SAFETY: Matches the SDK's pasteboard:item:provideDataForType: selector.
        #[unsafe(method(pasteboard:item:provideDataForType:))]
        fn provide(&self, _: Option<&NSPasteboard>, item: &NSPasteboardItem, requested: &NSString) {
            let context = Arc::clone(self.ivars());
            let address = item as *const NSPasteboardItem as usize;
            let requested = requested.to_string();
            let deadline = Instant::now() + MAIN_WAIT;
            let _ = on_main(MAIN_WAIT, move |_| {
                if Instant::now() < deadline { context.provide(address, requested); }
            });
        }
        // SAFETY: Matches the SDK's optional finished-provider selector.
        #[unsafe(method(pasteboardFinishedWithDataProvider:))]
        fn finished(&self, _: &NSPasteboard) {
            let context = Arc::clone(self.ivars());
            if !context.claim_finished() {
                return;
            }
            // AppKit may redeliver finished during a pasteboard call. Never re-enter
            // that pasteboard until this callback has returned.
            crate::main_thread::spawn_on_main(move |_| {
                let _ = on_main(MAIN_WAIT, move |_| {
                    context.complete(context.name.board().changeCount());
                    NATIVE.with_borrow_mut(|items| items.remove(&context.key()));
                });
            });
        }
    }
);

impl Context {
    pub(super) fn new(host: &MacClipboard, offer: u64, kinds: ClipKinds) -> Arc<Self> {
        Arc::new(Self {
            gate: Arc::clone(&host.gate),
            shared: Arc::clone(&host.shared),
            name: host.name.clone(),
            marker: host.marker.clone(),
            offer,
            kinds,
            written: AtomicIsize::new(-1),
            finished: AtomicBool::new(false),
            abandoned: AtomicBool::new(false),
        })
    }
    fn owner(&self) -> (u64, isize) {
        (self.offer, self.written.load(Ordering::Acquire))
    }
    fn key(&self) -> (String, isize) {
        (self.marker.clone(), self.owner().1)
    }
    fn claim_finished(&self) -> bool {
        !self.finished.swap(true, Ordering::AcqRel)
    }
    fn complete(&self, current: isize) {
        // AppKit also finishes a fully fulfilled provider without ending ownership.
        self.finished.store(true, Ordering::Release);
        if current != self.owner().1 {
            self.shared.lost(self.owner(), true);
        }
    }
    pub(super) fn rollback(&self) {
        let _ = withdraw(self.marker.clone(), Arc::clone(&self.shared), self.owner());
    }
    fn completion(&self, deadline: Instant) -> Result<(), PlatformError> {
        if self.abandoned.load(Ordering::Acquire) || Instant::now() >= deadline {
            Err(PlatformError::Timeout)
        } else {
            Ok(())
        }
    }

    pub(super) fn install(
        self: Arc<Self>,
        epoch: u64,
        deadline: Instant,
    ) -> Result<(), PlatformError> {
        autoreleasepool(|_| {
            check_read(&self.gate, epoch, deadline)?;
            if !self.shared.alive.load(Ordering::Acquire) {
                return Err(PlatformError::NotFound);
            }
            let this = Provider::alloc().set_ivars(Arc::clone(&self));
            // SAFETY: Correct NSObject initializer after initializing its Rust ivars.
            let provider: Retained<Provider> = unsafe { msg_send![super(this), init] };
            let item = NSPasteboardItem::new();
            let mut types = vec![NSString::from_str(&self.marker)];
            if self.kinds.text {
                types.push(NSString::from_str(TEXT));
            }
            if self.kinds.image {
                types.push(NSString::from_str(PNG));
            }
            let refs: Vec<&NSString> = types.iter().map(|value| &**value).collect();
            if !item.setDataProvider_forTypes(
                ProtocolObject::from_ref(&*provider),
                &NSArray::from_slice(&refs),
            ) {
                return Err(PlatformError::Backend("clipboard provider refused".into()));
            }
            check_read(&self.gate, epoch, deadline)?;
            let previous = self.shared.state.lock().map_err(|_| poisoned())?.own;
            let board = self.name.board();
            self.written.store(board.clearContents(), Ordering::Release);
            if let Some(previous) = previous {
                self.shared.lost(previous, true);
            }
            check_read(&self.gate, epoch, deadline)?;
            let object: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&*item);
            if !board.writeObjects(&NSArray::from_slice(&[object])) {
                return Err(PlatformError::Backend("clipboard promise refused".into()));
            }
            let mut state = self.shared.state.lock().map_err(|_| poisoned())?;
            let installed = state.installed(
                self.owner(),
                snapshot(&board, &self.marker, None),
                board.changeCount(),
            );
            drop(state);
            if !installed {
                return Err(PlatformError::Backend("clipboard ownership changed".into()));
            }
            NATIVE.with_borrow_mut(|items| items.insert(self.key(), (item, provider)));
            let result = self.completion(deadline);
            deliver_install(&self.gate, epoch, result, || self.rollback())
        })
    }

    fn provide(&self, address: usize, requested: String) {
        autoreleasepool(|_| {
            let Some((item, _provider)) =
                NATIVE.with_borrow(|items| items.get(&self.key()).cloned())
            else {
                return;
            };
            // Compare identities only: the address transported through dispatch is never dereferenced.
            if &*item as *const NSPasteboardItem as usize != address
                || self.finished.load(Ordering::Acquire)
            {
                return;
            }
            let requested_type = NSString::from_str(&requested);
            if requested == self.marker {
                item.setData_forType(&NSData::with_bytes(&[]), &requested_type);
                return;
            }
            let kind = match requested.as_str() {
                TEXT if self.kinds.text => ClipKind::Text,
                PNG if self.kinds.image => ClipKind::Image,
                _ => return,
            };
            let epoch = self.gate.epoch();
            if !self.gate.is_open() || !self.shared.alive.load(Ordering::Acquire) {
                return;
            }
            if WAITING.replace(true) {
                item.setData_forType(&NSData::with_bytes(&[]), &requested_type);
                return;
            }
            let _waiting = Wait;
            let Some(paste) = self.request(kind, epoch) else {
                return;
            };
            let deadline = Instant::now() + paste::LIMIT;
            loop {
                let answer = self.shared.poll_paste(paste, &self.gate);
                if let Some(answer) = answer {
                    if let Some(bytes) = answer {
                        let data = NSData::with_bytes(&bytes);
                        if self.deliverable(paste, epoch) {
                            item.setData_forType(&data, &requested_type);
                        }
                    }
                    break;
                }
                if Instant::now() >= deadline {
                    break;
                }
                let until = Instant::now()
                    + paste::SLICE.min(deadline.saturating_duration_since(Instant::now()));
                // SAFETY: Public default-mode constant, used only on the main thread.
                let mode = unsafe { NSDefaultRunLoopMode };
                NSRunLoop::mainRunLoop().runMode_beforeDate(
                    mode,
                    &NSDate::dateWithTimeIntervalSinceNow(
                        until
                            .saturating_duration_since(Instant::now())
                            .as_secs_f64(),
                    ),
                );
                // A run loop without ready sources may return early; avoid a busy spin.
                std::thread::sleep(until.saturating_duration_since(Instant::now()));
            }
            if let Ok(mut pastes) = self.shared.pastes.lock() {
                pastes.finish(paste);
            }
        });
    }

    fn request(&self, kind: ClipKind, epoch: u64) -> Option<LocalPasteId> {
        if self.name.board().changeCount() != self.owner().1 {
            self.shared.lost(self.owner(), true);
            return None;
        }
        let state = self.shared.state.lock().ok()?;
        if state.own != Some(self.owner()) {
            return None;
        }
        if !self.gate.is_open()
            || self.gate.epoch() != epoch
            || !self.shared.alive.load(Ordering::Acquire)
        {
            return None;
        }
        let sink = state.sink.as_ref()?;
        let paste = self.shared.begin_paste(epoch)?;
        sink.send(ClipboardEvent::PasteRequested {
            paste,
            offer: self.offer,
            kind,
        });
        Some(paste)
    }

    fn deliverable(&self, paste: LocalPasteId, epoch: u64) -> bool {
        self.name.board().changeCount() == self.owner().1
            && self
                .shared
                .state
                .lock()
                .is_ok_and(|state| state.own == Some(self.owner()))
            && self
                .shared
                .pastes
                .lock()
                .is_ok_and(|pastes| pastes.valid_delivery(paste, Instant::now(), epoch))
            && self.shared.alive.load(Ordering::Acquire)
            && self.gate.is_open()
            && self.gate.epoch() == epoch
    }
}

pub(super) fn withdraw(
    marker: String,
    shared: Arc<Shared>,
    owner: (u64, isize),
) -> Result<(), PlatformError> {
    shared.lost(owner, false);
    on_default(move |_| {
        NATIVE.with_borrow_mut(|items| items.remove(&(marker, owner.1)));
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reentrant_wait_is_process_wide_on_main_and_guard_clears_it() {
        assert!(!WAITING.replace(true));
        let guard = Wait;
        assert!(WAITING.replace(true));
        assert!(WAITING.get());
        drop(guard);
        assert!(!WAITING.get());
    }

    #[test]
    fn fulfilled_then_finished_still_owns_the_generation_for_withdrawal() {
        use super::super::tests::{fixture, open_gate};
        let mut host = fixture(open_gate());
        host.shared.state.lock().unwrap().own = Some((7, 101));
        let context = Context::new(
            &host,
            7,
            ClipKinds {
                text: true,
                image: false,
            },
        );
        context.written.store(101, Ordering::Release);
        let id = host.shared.begin_paste(host.gate.epoch()).unwrap();
        host.fulfil(id, Some(b"finished private fixture".to_vec()));
        assert!(host.shared.poll_paste(id, &host.gate).unwrap().is_some());
        host.shared.pastes.lock().unwrap().finish(id);
        context.complete(101);
        assert!(context.finished.load(Ordering::Acquire));
        assert_eq!(host.shared.prepare_withdraw(7).unwrap(), Some((7, 101)));
        host.shared.lost((7, 101), true);
        assert!(host.shared.state.lock().unwrap().own.is_none());
    }

    #[test]
    fn finished_after_actual_ownership_loss_notifies_once_and_cancels() {
        use super::super::tests::{fixture, open_gate};
        let host = fixture(open_gate());
        let (tx, rx) = mpsc::channel();
        {
            let mut state = host.shared.state.lock().unwrap();
            state.own = Some((7, 101));
            state.sink = Some(Arc::new(move |event| tx.send(event).unwrap()));
        }
        let context = Context::new(
            &host,
            7,
            ClipKinds {
                text: true,
                image: false,
            },
        );
        context.written.store(101, Ordering::Release);
        let id = host.shared.begin_paste(host.gate.epoch()).unwrap();
        context.complete(102);
        context.complete(102);
        assert_eq!(
            rx.try_recv().unwrap(),
            ClipboardEvent::PromiseLost { offer: 7 }
        );
        assert!(rx.try_recv().is_err());
        assert!(host.shared.state.lock().unwrap().own.is_none());
        assert_eq!(host.shared.poll_paste(id, &host.gate), Some(None));
    }

    fn installed_fixture() -> (MacClipboard, Arc<Context>) {
        use super::super::tests::{fixture, open_gate};
        let host = fixture(open_gate());
        host.shared.state.lock().unwrap().own = Some((7, 101));
        let context = Context::new(
            &host,
            7,
            ClipKinds {
                text: true,
                image: false,
            },
        );
        context.written.store(101, Ordering::Release);
        (host, context)
    }

    #[test]
    fn reentrant_finished_from_change_count_reads_and_completes_exactly_once() {
        fn finished(context: &Context, reads: &Cell<usize>, completed: &Cell<usize>) {
            if !context.claim_finished() {
                return;
            }
            reads.set(reads.get() + 1);
            // AppKit can redeliver finished from changeCount, before complete marks it.
            if reads.get() == 1 {
                finished(context, reads, completed);
            }
            context.complete(102);
            completed.set(completed.get() + 1);
        }

        let (host, context) = installed_fixture();
        let (tx, rx) = mpsc::channel();
        host.shared.state.lock().unwrap().sink =
            Some(Arc::new(move |event| tx.send(event).unwrap()));
        let id = host.shared.begin_paste(host.gate.epoch()).unwrap();
        let reads = Cell::new(0);
        let completed = Cell::new(0);
        finished(&context, &reads, &completed);
        finished(&context, &reads, &completed); // An independent duplicate delivery.
        assert_eq!(reads.get(), 1);
        assert_eq!(completed.get(), 1);
        assert_eq!(
            rx.try_recv().unwrap(),
            ClipboardEvent::PromiseLost { offer: 7 }
        );
        assert!(rx.try_recv().is_err());
        assert!(host.shared.state.lock().unwrap().own.is_none());
        assert_eq!(host.shared.poll_paste(id, &host.gate), Some(None));
    }

    #[test]
    fn native_install_that_outlives_deadline_is_rolled_back() {
        let (host, context) = installed_fixture();
        // writeObjects finished with a stable generation, after the caller's deadline.
        let result = context.completion(Instant::now());
        let cleaned = Cell::new(false);
        let result = deliver_install(&host.gate, host.gate.epoch(), result, || {
            host.shared.lost(context.owner(), false);
            cleaned.set(true);
        });
        assert!(matches!(result, Err(PlatformError::Timeout)));
        assert!(cleaned.get());
        assert!(host.shared.state.lock().unwrap().own.is_none());
    }

    #[test]
    fn caller_abandonment_between_write_and_completion_disables_generation() {
        let (host, context) = installed_fixture();
        context.abandoned.store(true, Ordering::Release);
        let result = context.completion(Instant::now() + MAIN_WAIT);
        let cleaned = Cell::new(false);
        let result = deliver_install(&host.gate, host.gate.epoch(), result, || {
            host.shared.lost(context.owner(), false);
            cleaned.set(true);
        });
        assert!(matches!(result, Err(PlatformError::Timeout)));
        assert!(cleaned.get());
        assert!(host.shared.state.lock().unwrap().own.is_none());
    }

    #[test]
    fn failed_completion_delivery_rolls_back_with_an_unchanged_gate() {
        let (host, context) = installed_fixture();
        // The native task completed, but its result could not reach the waiting caller.
        let result = Err(PlatformError::Timeout);
        context.abandoned.store(true, Ordering::Release);
        let cleaned = Cell::new(false);
        let result = deliver_install(&host.gate, host.gate.epoch(), result, || {
            host.shared.lost(context.owner(), false);
            cleaned.set(true);
        });
        assert!(matches!(result, Err(PlatformError::Timeout)));
        assert!(cleaned.get());
        assert!(host.shared.state.lock().unwrap().own.is_none());
    }
}
