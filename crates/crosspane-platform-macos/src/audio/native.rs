//! The production [`Hal`]: public CoreAudio calls only.
//!
//! - Reads: `AudioObjectGetPropertyData[Size]` on public property addresses.
//! - Notifications: `AudioObjectAddPropertyListenerBlock` onto this module's own serial dispatch
//!   queue (so delivery never depends on a run loop). The block captures an `Arc<Notifier>`, so the
//!   state it touches is reference counted by the block itself and cannot be freed while CoreAudio
//!   still holds the block, whatever the remove call's timing. The block only sets a flag and wakes
//!   the owner thread.
//! - IO: `AudioDeviceCreateIOProcID`/`Start`/`Stop`/`DestroyIOProcID` with a plain function
//!   pointer. Its client data is a leaked `Box<Registration>` freed only after the proc is
//!   destroyed *and* the callback reports idle; otherwise it is deliberately leaked.
//!
//! Nothing here sets a property, changes a default, or opens a device the host did not name by id.

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2_core_foundation::{CFRetained, CFString};

use super::callback::silence;
use super::ffi::{
    self, AudioBuffer, AudioBufferList, AudioDeviceIOProc, AudioDeviceIOProcID, AudioObjectID,
    AudioObjectPropertyAddress, AudioStreamBasicDescription, OSStatus,
};
use super::hal::{
    DeviceId, DeviceInfo, Hal, HalError, IoBuffer, IoCallback, IoSession, ListenTarget, ListenerId,
    Notifier, StreamFormat, StreamId, StreamInfo,
};

/// The most buffers one IO cycle may carry; a list with more is rejected (never truncated).
const MAX_BUFFERS: usize = 16;
/// The most stream ids one device may report.
const MAX_STREAMS: usize = 64;

fn status(code: OSStatus) -> Result<(), HalError> {
    match code {
        ffi::kAudioHardwareNoError => Ok(()),
        ffi::kAudioHardwareBadObjectError
        | ffi::kAudioHardwareBadDeviceError
        | ffi::kAudioHardwareBadStreamError => Err(HalError::Gone),
        ffi::kAudioDevicePermissionsError => Err(HalError::Denied),
        other => Err(HalError::Status(other)),
    }
}

fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: ffi::kAudioObjectPropertyElementMain,
    }
}

/// Read a fixed-size property value whose size must match exactly.
fn get_scalar<T: Copy + Default>(
    object: AudioObjectID,
    address: AudioObjectPropertyAddress,
) -> Result<T, HalError> {
    let mut value = T::default();
    let mut size = size_of::<T>() as u32;
    // SAFETY: `address` and `size` are valid for the call; `value` is a writable `T` of exactly
    // `size` bytes; no qualifier is passed.
    let code = unsafe {
        ffi::AudioObjectGetPropertyData(
            object,
            &address,
            0,
            ptr::null(),
            &mut size,
            ptr::from_mut(&mut value).cast(),
        )
    };
    status(code)?;
    if size as usize != size_of::<T>() {
        return Err(HalError::Status(ffi::kAudioHardwareBadPropertySizeError));
    }
    Ok(value)
}

/// Read an array-of-u32 property (stream lists).
fn get_u32_list(
    object: AudioObjectID,
    address: AudioObjectPropertyAddress,
) -> Result<Vec<u32>, HalError> {
    let mut size = 0u32;
    // SAFETY: `address` and `size` are valid for the call; no qualifier is passed.
    let code =
        unsafe { ffi::AudioObjectGetPropertyDataSize(object, &address, 0, ptr::null(), &mut size) };
    status(code)?;
    let bytes = size as usize;
    if !bytes.is_multiple_of(4) || bytes / 4 > MAX_STREAMS {
        return Err(HalError::Status(ffi::kAudioHardwareBadPropertySizeError));
    }
    let mut out = vec![0u32; bytes / 4];
    if out.is_empty() {
        return Ok(out);
    }
    let mut io_size = size;
    // SAFETY: `out` is writable for `io_size` bytes; `address` and `io_size` are valid for the call.
    let code = unsafe {
        ffi::AudioObjectGetPropertyData(
            object,
            &address,
            0,
            ptr::null(),
            &mut io_size,
            out.as_mut_ptr().cast(),
        )
    };
    status(code)?;
    out.truncate(io_size as usize / 4);
    Ok(out)
}

/// `kAudioDevicePropertyDeviceUID`: a CFString the caller owns.
fn device_uid(device: AudioObjectID) -> Result<String, HalError> {
    let addr = address(
        ffi::kAudioDevicePropertyDeviceUID,
        ffi::kAudioObjectPropertyScopeGlobal,
    );
    let mut raw: *const CFString = ptr::null();
    let mut size = size_of::<*const CFString>() as u32;
    // SAFETY: `raw` is a writable pointer-sized slot and `size` says so; the property returns a
    // retained CFStringRef or leaves null.
    let code = unsafe {
        ffi::AudioObjectGetPropertyData(
            device,
            &addr,
            0,
            ptr::null(),
            &mut size,
            ptr::from_mut(&mut raw).cast(),
        )
    };
    status(code)?;
    let raw = NonNull::new(raw.cast_mut()).ok_or(HalError::Invalid)?;
    // SAFETY: DeviceUID follows the CF "Get" rule for properties: the caller owns the returned
    // reference (+1), which `CFRetained` now releases on drop.
    let uid = unsafe { CFRetained::from_raw(raw) };
    Ok(uid.to_string())
}

fn format(asbd: AudioStreamBasicDescription) -> StreamFormat {
    StreamFormat {
        sample_rate: asbd.mSampleRate,
        format_id: asbd.mFormatID,
        format_flags: asbd.mFormatFlags,
        bytes_per_packet: asbd.mBytesPerPacket,
        frames_per_packet: asbd.mFramesPerPacket,
        bytes_per_frame: asbd.mBytesPerFrame,
        channels_per_frame: asbd.mChannelsPerFrame,
        bits_per_channel: asbd.mBitsPerChannel,
        reserved: asbd.mReserved,
    }
}

fn streams(device: AudioObjectID, scope: u32) -> Result<Vec<StreamInfo>, HalError> {
    get_u32_list(device, address(ffi::kAudioDevicePropertyStreams, scope))?
        .into_iter()
        .map(|id| {
            let asbd: AudioStreamBasicDescription = get_scalar(
                id,
                address(
                    ffi::kAudioStreamPropertyVirtualFormat,
                    ffi::kAudioObjectPropertyScopeGlobal,
                ),
            )?;
            Ok(StreamInfo {
                id: StreamId(id),
                format: format(asbd),
            })
        })
        .collect()
}

fn listener_address(target: ListenTarget) -> (AudioObjectID, AudioObjectPropertyAddress) {
    let global = ffi::kAudioObjectPropertyScopeGlobal;
    match target {
        ListenTarget::DeviceRunning(d) => (
            d.0,
            address(ffi::kAudioDevicePropertyDeviceIsRunningSomewhere, global),
        ),
        ListenTarget::DeviceAlive(d) => {
            (d.0, address(ffi::kAudioDevicePropertyDeviceIsAlive, global))
        }
        ListenTarget::DeviceNominalRate(d) => (
            d.0,
            address(ffi::kAudioDevicePropertyNominalSampleRate, global),
        ),
        ListenTarget::DeviceOutputConfig(d) => (
            d.0,
            address(
                ffi::kAudioDevicePropertyStreamConfiguration,
                ffi::kAudioObjectPropertyScopeOutput,
            ),
        ),
        ListenTarget::StreamFormat(s) => {
            (s.0, address(ffi::kAudioStreamPropertyVirtualFormat, global))
        }
        ListenTarget::DefaultOutput => (
            ffi::kAudioObjectSystemObject,
            address(ffi::kAudioHardwarePropertyDefaultOutputDevice, global),
        ),
        ListenTarget::DefaultSystemOutput => (
            ffi::kAudioObjectSystemObject,
            address(ffi::kAudioHardwarePropertyDefaultSystemOutputDevice, global),
        ),
        ListenTarget::DeviceList => (
            ffi::kAudioObjectSystemObject,
            address(ffi::kAudioHardwarePropertyDevices, global),
        ),
        ListenTarget::ServiceRestarted => (
            ffi::kAudioObjectSystemObject,
            address(ffi::kAudioHardwarePropertyServiceRestarted, global),
        ),
    }
}

struct NativeListener {
    object: AudioObjectID,
    address: AudioObjectPropertyAddress,
    block: RcBlock<dyn Fn(u32, *const c_void)>,
}

// SAFETY: the block's closure captures only an `Arc<Notifier>` (Send + Sync) and never touches
// thread-local state; block copies and releases are atomic, and the block is only ever invoked by
// CoreAudio on this module's dispatch queue. The `RcBlock` handle itself is moved between threads
// only to call `Remove` and to drop.
unsafe impl Send for NativeListener {}

/// The production HAL. Creating it makes no CoreAudio call.
pub(super) struct NativeHal {
    queue: DispatchRetained<DispatchQueue>,
    listeners: Mutex<HashMap<u64, NativeListener>>,
    next_listener: AtomicU64,
    procs: Arc<dyn ProcApi>,
}

impl NativeHal {
    pub(super) fn new() -> Self {
        Self::with_procs(Arc::new(CoreAudioProcs))
    }

    /// The raw IOProc calls are injectable so failure cleanup runs without a device.
    fn with_procs(procs: Arc<dyn ProcApi>) -> Self {
        Self {
            queue: DispatchQueue::new("io.frostdev.crosspane.audio.hal-listeners", None),
            listeners: Mutex::new(HashMap::new()),
            next_listener: AtomicU64::new(1),
            procs,
        }
    }
}

impl Drop for NativeHal {
    fn drop(&mut self) {
        // The host removes its listeners before dropping the HAL; this only covers a wedged
        // owner thread. CoreAudio keeps its own references to the blocks and the queue.
        let listeners = std::mem::take(
            &mut *self
                .listeners
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for (_, entry) in listeners {
            // SAFETY: the same object, address, queue and block that were registered.
            let _ = unsafe {
                ffi::AudioObjectRemovePropertyListenerBlock(
                    entry.object,
                    &entry.address,
                    &self.queue,
                    &entry.block,
                )
            };
        }
    }
}

impl Hal for NativeHal {
    fn translate_uid(&self, uid: &str) -> Result<Option<DeviceId>, HalError> {
        let qualifier = CFString::from_str(uid);
        let qualifier_ref: *const CFString = &*qualifier;
        let addr = address(
            ffi::kAudioHardwarePropertyTranslateUIDToDevice,
            ffi::kAudioObjectPropertyScopeGlobal,
        );
        let mut device: AudioObjectID = ffi::kAudioObjectUnknown;
        let mut size = size_of::<AudioObjectID>() as u32;
        // SAFETY: the qualifier is the address of a CFStringRef-sized value that points at a live
        // CFString (`qualifier` outlives the call); `device`/`size` describe a writable u32.
        let code = unsafe {
            ffi::AudioObjectGetPropertyData(
                ffi::kAudioObjectSystemObject,
                &addr,
                size_of::<*const CFString>() as u32,
                ptr::from_ref(&qualifier_ref).cast(),
                &mut size,
                ptr::from_mut(&mut device).cast(),
            )
        };
        status(code)?;
        Ok((device != ffi::kAudioObjectUnknown).then_some(DeviceId(device)))
    }

    fn device_info(&self, device: DeviceId) -> Result<DeviceInfo, HalError> {
        let global = ffi::kAudioObjectPropertyScopeGlobal;
        let d = device.0;
        let hidden = match get_scalar::<u32>(d, address(ffi::kAudioDevicePropertyIsHidden, global))
        {
            Ok(value) => value != 0,
            // Devices that do not implement the property are not hidden.
            Err(HalError::Status(ffi::kAudioHardwareUnknownPropertyError)) => false,
            Err(error) => return Err(error),
        };
        Ok(DeviceInfo {
            uid: device_uid(d)?,
            class_id: get_scalar(d, address(ffi::kAudioObjectPropertyClass, global))?,
            transport: get_scalar(d, address(ffi::kAudioDevicePropertyTransportType, global))?,
            alive: get_scalar::<u32>(d, address(ffi::kAudioDevicePropertyDeviceIsAlive, global))?
                != 0,
            hidden,
            nominal_rate: get_scalar(
                d,
                address(ffi::kAudioDevicePropertyNominalSampleRate, global),
            )?,
            input_streams: streams(d, ffi::kAudioObjectPropertyScopeInput)?,
            output_streams: streams(d, ffi::kAudioObjectPropertyScopeOutput)?,
        })
    }

    fn is_alive(&self, device: DeviceId) -> Result<bool, HalError> {
        let addr = address(
            ffi::kAudioDevicePropertyDeviceIsAlive,
            ffi::kAudioObjectPropertyScopeGlobal,
        );
        Ok(get_scalar::<u32>(device.0, addr)? != 0)
    }

    fn is_running_somewhere(&self, device: DeviceId) -> Result<bool, HalError> {
        let addr = address(
            ffi::kAudioDevicePropertyDeviceIsRunningSomewhere,
            ffi::kAudioObjectPropertyScopeGlobal,
        );
        Ok(get_scalar::<u32>(device.0, addr)? != 0)
    }

    fn default_output_device(&self) -> Result<Option<DeviceId>, HalError> {
        let addr = address(
            ffi::kAudioHardwarePropertyDefaultOutputDevice,
            ffi::kAudioObjectPropertyScopeGlobal,
        );
        let device: AudioObjectID = get_scalar(ffi::kAudioObjectSystemObject, addr)?;
        Ok((device != ffi::kAudioObjectUnknown).then_some(DeviceId(device)))
    }

    fn default_system_output_device(&self) -> Result<Option<DeviceId>, HalError> {
        let addr = address(
            ffi::kAudioHardwarePropertyDefaultSystemOutputDevice,
            ffi::kAudioObjectPropertyScopeGlobal,
        );
        let device: AudioObjectID = get_scalar(ffi::kAudioObjectSystemObject, addr)?;
        Ok((device != ffi::kAudioObjectUnknown).then_some(DeviceId(device)))
    }

    fn device_ids(&self) -> Result<Vec<DeviceId>, HalError> {
        let addr = address(
            ffi::kAudioHardwarePropertyDevices,
            ffi::kAudioObjectPropertyScopeGlobal,
        );
        Ok(get_u32_list(ffi::kAudioObjectSystemObject, addr)?
            .into_iter()
            .map(DeviceId)
            .collect())
    }

    fn add_listener(
        &self,
        target: ListenTarget,
        notifier: Arc<Notifier>,
    ) -> Result<ListenerId, HalError> {
        let (object, address) = listener_address(target);
        let block: RcBlock<dyn Fn(u32, *const c_void)> =
            RcBlock::new(move |_count: u32, _addresses: *const c_void| notifier.notify());
        // SAFETY: `address` is valid for the call; the queue outlives the registration (it is
        // owned by this HAL, which removes every listener before it is dropped); CoreAudio
        // copies the block and keeps it until the matching remove.
        let code = unsafe {
            ffi::AudioObjectAddPropertyListenerBlock(object, &address, &self.queue, &block)
        };
        status(code)?;
        let id = self.next_listener.fetch_add(1, Ordering::Relaxed);
        self.listeners
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                id,
                NativeListener {
                    object,
                    address,
                    block,
                },
            );
        Ok(ListenerId(id))
    }

    fn remove_listener(&self, listener: ListenerId) -> Result<(), HalError> {
        let entry = self
            .listeners
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&listener.0)
            .ok_or(HalError::Gone)?;
        // SAFETY: the same object, address, queue and block that were registered.
        let code = unsafe {
            ffi::AudioObjectRemovePropertyListenerBlock(
                entry.object,
                &entry.address,
                &self.queue,
                &entry.block,
            )
        };
        // A vanished object has nothing registered any more. Dropping `entry` releases only our
        // reference; CoreAudio's copy keeps the closure alive for any notification in flight.
        match status(code) {
            Ok(()) | Err(HalError::Gone) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn flush_listeners(&self, timeout: Duration) -> bool {
        let (tx, rx) = mpsc::sync_channel::<()>(1);
        self.queue.exec_async(move || {
            let _ = tx.try_send(());
        });
        rx.recv_timeout(timeout).is_ok()
    }

    fn start_io(
        &self,
        device: DeviceId,
        callback: Arc<dyn IoCallback>,
    ) -> Result<Box<dyn IoSession>, HalError> {
        start_registered(&self.procs, device.0, callback)
    }
}

/// The four raw IOProc calls. Production calls CoreAudio; tests inject failures so the real
/// cleanup logic below runs without a device.
trait ProcApi: Send + Sync {
    /// `AudioDeviceCreateIOProcID` with [`trampoline`] and `client` as client data.
    fn create(
        &self,
        device: AudioObjectID,
        client: *mut c_void,
    ) -> Result<AudioDeviceIOProc, HalError>;
    /// `AudioDeviceStart`.
    fn start(&self, device: AudioObjectID, proc_id: AudioDeviceIOProc) -> Result<(), HalError>;
    /// `AudioDeviceStop`.
    fn stop(&self, device: AudioObjectID, proc_id: AudioDeviceIOProc) -> Result<(), HalError>;
    /// `AudioDeviceDestroyIOProcID`.
    fn destroy(&self, device: AudioObjectID, proc_id: AudioDeviceIOProc) -> Result<(), HalError>;
}

struct CoreAudioProcs;

impl ProcApi for CoreAudioProcs {
    fn create(
        &self,
        device: AudioObjectID,
        client: *mut c_void,
    ) -> Result<AudioDeviceIOProc, HalError> {
        let mut proc_id: AudioDeviceIOProcID = None;
        // SAFETY: `trampoline` matches `AudioDeviceIOProc`; `client` stays allocated until the proc
        // is destroyed and drained (see `NativeIo::drop`); `proc_id` is a writable slot.
        let code =
            unsafe { ffi::AudioDeviceCreateIOProcID(device, trampoline, client, &mut proc_id) };
        status(code)?;
        proc_id.ok_or(HalError::Invalid)
    }

    fn start(&self, device: AudioObjectID, proc_id: AudioDeviceIOProc) -> Result<(), HalError> {
        // SAFETY: the proc id was created on this device and not yet destroyed.
        status(unsafe { ffi::AudioDeviceStart(device, proc_id) })
    }

    fn stop(&self, device: AudioObjectID, proc_id: AudioDeviceIOProc) -> Result<(), HalError> {
        // SAFETY: as above.
        status(unsafe { ffi::AudioDeviceStop(device, proc_id) })
    }

    fn destroy(&self, device: AudioObjectID, proc_id: AudioDeviceIOProc) -> Result<(), HalError> {
        // SAFETY: as above.
        status(unsafe { ffi::AudioDeviceDestroyIOProcID(device, proc_id) })
    }
}

/// How long retirement waits for trampolines that are inside the registration.
const DRAIN_WAIT: Duration = Duration::from_millis(100);

/// The IOProc's client data.
///
/// **A `Registration` is never freed.** It is leaked (`Box::leak`) when the proc is created, so a
/// trampoline that starts at any later time, however late, only ever touches memory that is still
/// allocated. Its three atomics (`inflight`, `disabled`) are all the trampoline reads before it
/// knows whether it may proceed. The callback state it guards (rings, buffers) lives behind
/// `callback` and is released only by [`release_state`](Self::release_state), once retirement has
/// proven that no trampoline can reach it; if that cannot be proven the state is leaked as well.
struct Registration {
    /// Trampolines that have announced themselves (their very first action) and not yet left.
    inflight: AtomicU32,
    /// Set by retirement before the proc is destroyed. A trampoline that finds it set returns
    /// silently without touching `callback`.
    disabled: AtomicBool,
    /// The callback state. Written only by `release_state` (after `disabled` is set and `inflight`
    /// has been seen at zero), read by trampolines that entered with `disabled` clear and by the
    /// owner's retirement.
    callback: UnsafeCell<Option<Arc<dyn IoCallback>>>,
}

// SAFETY: `inflight` and `disabled` are atomics. `callback` is only mutated by `release_state`,
// whose contract (disabled, destroyed, `inflight == 0` observed after `disabled` was stored,
// sequentially consistent) means no other thread can be reading it; concurrent readers
// (`callback`) only share-read an `Option<Arc<dyn IoCallback>>`, and `IoCallback: Send + Sync`.
unsafe impl Sync for Registration {}

/// Proof that a trampoline is inside; dropping it leaves.
struct Entry<'a>(&'a Registration);

impl Drop for Entry<'_> {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Registration {
    fn leak(callback: Arc<dyn IoCallback>) -> &'static Registration {
        Box::leak(Box::new(Registration {
            inflight: AtomicU32::new(0),
            disabled: AtomicBool::new(false),
            callback: UnsafeCell::new(Some(callback)),
        }))
    }

    /// A trampoline announces itself. Must be its first action.
    fn enter(&self) -> Entry<'_> {
        self.inflight.fetch_add(1, Ordering::SeqCst);
        Entry(self)
    }

    fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::SeqCst)
    }

    /// The callback state, for a holder that is entered and saw `disabled` clear, or for the
    /// owner while the state has not been released.
    ///
    /// # Safety
    ///
    /// The caller is an [`Entry`] holder that observed `is_disabled() == false` after entering, or
    /// the owner thread before it has called `release_state`. Either way `release_state` cannot
    /// run concurrently.
    unsafe fn callback(&self) -> Option<&Arc<dyn IoCallback>> {
        // SAFETY: no concurrent mutation, per the caller.
        unsafe { (*self.callback.get()).as_ref() }
    }

    /// Retirement's first two actions: silence the callback itself, then close the registration's
    /// own gate. Owner thread only, before `release_state`.
    fn disable(&self) {
        // SAFETY: the owner has not released the state (this is called on the owner's session
        // before `Drop` ends), so it is alive and not being mutated.
        if let Some(callback) = unsafe { self.callback() } {
            callback.control().disable();
        }
        self.disabled.store(true, Ordering::SeqCst);
    }

    /// Wait (bounded) until no trampoline is inside.
    fn drain(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while self.inflight.load(Ordering::SeqCst) != 0 {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
        true
    }

    /// Free the callback state (the rings and buffers it owns), never the registration.
    ///
    /// # Safety
    ///
    /// `disable` has been called, the proc is destroyed (or never created), and a `drain` that
    /// began after `disabled` was stored has seen `inflight == 0`. Then every trampoline that
    /// could reach `callback` has already left, and every later one returns at the `disabled`
    /// check.
    unsafe fn release_state(&self) {
        // SAFETY: exclusive access, per the caller.
        drop(unsafe { (*self.callback.get()).take() });
    }
}

/// Create and start an IOProc whose client data is a fresh, leaked [`Registration`].
fn start_registered(
    api: &Arc<dyn ProcApi>,
    device: AudioObjectID,
    callback: Arc<dyn IoCallback>,
) -> Result<Box<dyn IoSession>, HalError> {
    let registration = Registration::leak(callback);
    let client = ptr::from_ref(registration).cast_mut().cast::<c_void>();
    let proc_fn = match api.create(device, client) {
        Ok(proc_fn) => proc_fn,
        Err(error) => {
            // No proc exists, so no trampoline can ever run: release the state now. The (small)
            // registration stays leaked like every other.
            registration.disable();
            // SAFETY: disabled, never published to a live proc, so `inflight` is zero for good.
            unsafe { registration.release_state() };
            return Err(error);
        }
    };
    let mut session = NativeIo {
        api: api.clone(),
        device,
        proc_fn,
        registration,
        started: false,
        destroyed: false,
    };
    match api.start(device, proc_fn) {
        Ok(()) => {
            session.started = true;
            Ok(Box::new(session))
        }
        // Dropping the session retires it: disable, destroy, drain, then release or leak the state.
        Err(error) => Err(error),
    }
}

/// A created (and normally started) IOProc.
struct NativeIo {
    api: Arc<dyn ProcApi>,
    device: AudioObjectID,
    proc_fn: AudioDeviceIOProc,
    registration: &'static Registration,
    started: bool,
    destroyed: bool,
}

impl NativeIo {
    /// Disable production and consumption, then stop and destroy the proc. The disable comes
    /// first on every attempt (explicit stop, failed start, drop), so a callback that is still
    /// running or about to run does nothing.
    fn retire(&mut self) -> Result<(), HalError> {
        self.registration.disable();
        if self.destroyed {
            return Ok(());
        }
        if self.started {
            self.started = false;
            // A failure here is not fatal: destroying the proc below also stops it.
            let _ = self.api.stop(self.device, self.proc_fn);
        }
        match self.api.destroy(self.device, self.proc_fn) {
            // `Gone`: the device vanished, so no registration remains to call us.
            Ok(()) | Err(HalError::Gone) => {
                self.destroyed = true;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

impl std::fmt::Debug for NativeIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeIo")
            .field("device", &self.device)
            .field("started", &self.started)
            .field("destroyed", &self.destroyed)
            .finish_non_exhaustive()
    }
}

impl IoSession for NativeIo {
    fn stop(&mut self) -> Result<(), HalError> {
        self.retire()
    }
}

impl Drop for NativeIo {
    fn drop(&mut self) {
        let _ = self.retire();
        // Release the callback state only when the proc is destroyed and no trampoline is inside;
        // any trampoline that starts later (even one that began before `destroy` returned and was
        // merely paused) finds `disabled` set and never reaches the state. Otherwise the state is
        // kept (disabled) forever. The registration itself is never freed.
        if self.destroyed && self.registration.drain(DRAIN_WAIT) {
            // SAFETY: `retire` stored `disabled`, the proc is destroyed, and `drain` (which ran
            // after that store) saw no trampoline inside.
            unsafe { self.registration.release_state() };
        } else {
            tracing::warn!("audio IOProc state kept alive: not provably quiescent");
        }
    }
}

/// Copy a HAL buffer list into `into`. `None` when the list is longer than `into`.
///
/// # Safety
///
/// `list` is null or points at an `AudioBufferList` whose `mNumberBuffers` buffers are readable.
unsafe fn copy_list(
    list: *const AudioBufferList,
    into: &mut [IoBuffer; MAX_BUFFERS],
) -> Option<usize> {
    if list.is_null() {
        return Some(0);
    }
    // SAFETY: `list` is non-null and readable per the caller.
    let count = unsafe { (*list).mNumberBuffers } as usize;
    if count > MAX_BUFFERS {
        return None;
    }
    // SAFETY: the buffer array starts at this field; its length is `count` per the caller.
    let first = unsafe { ptr::addr_of!((*list).mBuffers) }.cast::<AudioBuffer>();
    for (index, slot) in into.iter_mut().take(count).enumerate() {
        // SAFETY: `index < count`, so this buffer is inside the list.
        let buffer = unsafe { first.add(index).read() };
        *slot = IoBuffer {
            channels: buffer.mNumberChannels,
            byte_size: buffer.mDataByteSize,
            data: buffer.mData,
        };
    }
    Some(count)
}

/// The `AudioDeviceIOProc` handed to CoreAudio. Real-time: no allocation, no lock, no HAL call.
unsafe extern "C" fn trampoline(
    _device: AudioObjectID,
    _now: *const c_void,
    input: *const AudioBufferList,
    _input_time: *const c_void,
    output: *mut AudioBufferList,
    _output_time: *const c_void,
    client_data: *mut c_void,
) -> OSStatus {
    const EMPTY: IoBuffer = IoBuffer {
        channels: 0,
        byte_size: 0,
        data: ptr::null_mut(),
    };
    // SAFETY: `client_data` is the leaked `Registration` made in `start_registered`; registrations
    // are never freed, so this reference is valid however late this call runs.
    let registration = unsafe { &*client_data.cast::<Registration>() };
    // 1. Announce this trampoline: retirement waits for every announced trampoline.
    let _entry = registration.enter();
    let mut inputs = [EMPTY; MAX_BUFFERS];
    let mut outputs = [EMPTY; MAX_BUFFERS];
    // 2. Retired (or retiring)? Then return silent without touching the callback state, which may
    //    already be freed. Only the HAL's own output list is touched.
    if registration.is_disabled() {
        // SAFETY: the HAL passes null or a valid output list for the duration of the call.
        if let Some(count) = unsafe { copy_list(output.cast_const(), &mut outputs) } {
            // SAFETY: the buffers are the HAL's own, valid and writable for this call.
            unsafe { silence(&outputs[..count]) };
        }
        return ffi::kAudioHardwareNoError;
    }
    // 3. Entered with `disabled` clear: `release_state` cannot run until this entry is dropped.
    // SAFETY: as just argued.
    let Some(callback) = (unsafe { registration.callback() }) else {
        return ffi::kAudioHardwareNoError;
    };
    // SAFETY: the HAL passes null or valid lists for the duration of the call.
    let lists = unsafe {
        (
            copy_list(input, &mut inputs),
            copy_list(output.cast_const(), &mut outputs),
        )
    };
    match lists {
        (Some(inputs_len), Some(outputs_len)) => {
            // SAFETY: the buffers the HAL describes are valid for the call (input readable, output
            // writable) and not accessed elsewhere meanwhile.
            unsafe {
                callback.process(&inputs[..inputs_len], &outputs[..outputs_len]);
            }
        }
        // An absurd number of buffers: refuse the cycle rather than truncate it.
        _ => callback.control().mark_bad(),
    }
    ffi::kAudioHardwareNoError
}

#[cfg(test)]
mod tests {
    //! The parts of the native HAL that need no device: status mapping, address mapping, the
    //! IOProc trampoline's handling of a hand-built `AudioBufferList` (flexible array included),
    //! and the registration lifecycle (start, failed start, retirement, leak) through an injected
    //! [`ProcApi`]. No CoreAudio function is called.

    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    use super::*;
    use crate::audio::hal::CallbackControl;

    /// (channels, byte size, data address): addresses as integers so the record is `Send`.
    type Seen = (u32, u32, usize);

    fn seen(buffers: &[IoBuffer]) -> Vec<Seen> {
        buffers
            .iter()
            .map(|b| (b.channels, b.byte_size, b.data as usize))
            .collect()
    }

    #[derive(Debug, Default)]
    struct Recorder {
        control: CallbackControl,
        seen: Mutex<Vec<(Vec<Seen>, Vec<Seen>)>>,
        /// When set, `process` waits for `release` (a callback caught inside `process`).
        block: AtomicBool,
        inside: AtomicBool,
        release: AtomicBool,
        /// Address of the registration being driven, when `call` observes it.
        watched: AtomicUsize,
        /// The registration's in-flight count as seen from inside `process` and `control`.
        counted: Mutex<Vec<(&'static str, u32)>>,
    }

    impl Recorder {
        fn observe(&self, at: &'static str) {
            let address = self.watched.load(Ordering::SeqCst);
            if address != 0 {
                // SAFETY: `call` stores the address of a live `Registration` before the trampoline
                // runs and clears it after, on the same thread.
                let registration = unsafe { &*(address as *const Registration) };
                self.counted
                    .lock()
                    .unwrap()
                    .push((at, registration.inflight.load(Ordering::SeqCst)));
            }
        }
    }

    impl IoCallback for Recorder {
        unsafe fn process(&self, input: &[IoBuffer], output: &[IoBuffer]) {
            self.observe("process");
            self.seen.lock().unwrap().push((seen(input), seen(output)));
            self.inside.store(true, Ordering::SeqCst);
            if self.block.load(Ordering::SeqCst) {
                while !self.release.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }

        fn control(&self) -> &CallbackControl {
            self.observe("control");
            &self.control
        }
    }

    /// An `AudioBufferList` with `count` buffers in 8-byte-aligned storage, laid out as the SDK
    /// declares it: a `UInt32` count, padding, then the buffers.
    struct List {
        storage: Vec<u64>,
    }

    impl List {
        fn new(buffers: &[(u32, u32, *mut c_void)]) -> Self {
            let bytes = size_of::<AudioBufferList>() + buffers.len().saturating_sub(1) * 16;
            let mut storage = vec![0u64; bytes.div_ceil(8)];
            let base = storage.as_mut_ptr().cast::<u8>();
            // SAFETY: `storage` is at least `8 + 16 * count` bytes (the first buffer is part of
            // `AudioBufferList` itself) and 8-byte aligned; writes stay inside it.
            unsafe {
                base.cast::<u32>().write(buffers.len() as u32);
                for (index, (channels, size, data)) in buffers.iter().enumerate() {
                    base.add(8 + 16 * index)
                        .cast::<AudioBuffer>()
                        .write(AudioBuffer {
                            mNumberChannels: *channels,
                            mDataByteSize: *size,
                            mData: *data,
                        });
                }
            }
            Self { storage }
        }

        fn ptr(&mut self) -> *mut AudioBufferList {
            self.storage.as_mut_ptr().cast()
        }
    }

    /// Invoke the trampoline on a throwaway registration; returns the status and the in-flight
    /// count afterwards (which must be back to zero).
    fn call(
        callback: &Arc<Recorder>,
        input: *const AudioBufferList,
        output: *mut AudioBufferList,
    ) -> (OSStatus, u32) {
        let registration = Registration::leak(callback.clone());
        callback
            .watched
            .store(ptr::from_ref(registration) as usize, Ordering::SeqCst);
        // SAFETY: `registration` is a live `Registration`; the lists are null or valid for the
        // call, exactly as a HAL would pass them.
        let status =
            unsafe { raw_call(ptr::from_ref(registration).cast_mut().cast(), input, output) };
        callback.watched.store(0, Ordering::SeqCst);
        let inflight = registration.inflight.load(Ordering::SeqCst);
        registration.disable();
        // SAFETY: disabled, no proc exists, and the call has returned (`inflight` is zero).
        unsafe { registration.release_state() };
        (status, inflight)
    }

    /// # Safety
    ///
    /// `client` is a live `Registration`; the lists are null or valid.
    unsafe fn raw_call(
        client: *mut c_void,
        input: *const AudioBufferList,
        output: *mut AudioBufferList,
    ) -> OSStatus {
        // SAFETY: forwarded from this function's contract.
        unsafe {
            trampoline(
                7,
                ptr::null(),
                input,
                ptr::null(),
                output,
                ptr::null(),
                client,
            )
        }
    }

    #[test]
    fn statuses_map_to_the_documented_errors() {
        assert_eq!(status(0), Ok(()));
        for code in [
            ffi::kAudioHardwareBadObjectError,
            ffi::kAudioHardwareBadDeviceError,
            ffi::kAudioHardwareBadStreamError,
        ] {
            assert_eq!(status(code), Err(HalError::Gone));
        }
        assert_eq!(
            status(ffi::kAudioDevicePermissionsError),
            Err(HalError::Denied)
        );
        assert_eq!(status(-50), Err(HalError::Status(-50)));
    }

    #[test]
    fn listener_targets_map_to_public_addresses() {
        let global = ffi::kAudioObjectPropertyScopeGlobal;
        let (object, addr) = listener_address(ListenTarget::DeviceRunning(DeviceId(9)));
        assert_eq!(object, 9);
        assert_eq!(
            addr.mSelector,
            ffi::kAudioDevicePropertyDeviceIsRunningSomewhere
        );
        assert_eq!((addr.mScope, addr.mElement), (global, 0));
        let (object, addr) = listener_address(ListenTarget::DefaultOutput);
        assert_eq!(object, ffi::kAudioObjectSystemObject);
        assert_eq!(
            addr.mSelector,
            ffi::kAudioHardwarePropertyDefaultOutputDevice
        );
        let (_, addr) = listener_address(ListenTarget::DeviceOutputConfig(DeviceId(3)));
        assert_eq!(addr.mScope, ffi::kAudioObjectPropertyScopeOutput);
        let (object, addr) = listener_address(ListenTarget::StreamFormat(StreamId(4)));
        assert_eq!(object, 4);
        assert_eq!(addr.mSelector, ffi::kAudioStreamPropertyVirtualFormat);
    }

    #[test]
    fn the_trampoline_passes_every_buffer_of_both_lists_through() {
        let mut data = [0f32; 8];
        let mut other = [0f32; 4];
        let mut input = List::new(&[(2, 32, data.as_mut_ptr().cast())]);
        let mut output = List::new(&[
            (1, 16, other.as_mut_ptr().cast()),
            (1, 16, ptr::null_mut()),
            (2, 0, ptr::null_mut()),
        ]);
        let recorder = Arc::new(Recorder::default());
        assert_eq!(call(&recorder, input.ptr(), output.ptr()), (0, 0));
        // The trampoline counted itself in before reaching `process`.
        assert_eq!(*recorder.counted.lock().unwrap(), [("process", 1)]);
        let seen = recorder.seen.lock().unwrap();
        let (inputs, outputs) = &seen[0];
        assert_eq!(inputs, &[(2, 32, data.as_mut_ptr() as usize)]);
        assert_eq!(
            outputs,
            &[(1, 16, other.as_mut_ptr() as usize), (1, 16, 0), (2, 0, 0)]
        );
        assert!(!recorder.control.bad_buffer_seen());
    }

    #[test]
    fn null_lists_are_empty_lists() {
        let recorder = Arc::new(Recorder::default());
        assert_eq!(call(&recorder, ptr::null(), ptr::null_mut()), (0, 0));
        let seen = recorder.seen.lock().unwrap();
        assert!(seen[0].0.is_empty() && seen[0].1.is_empty());
    }

    #[test]
    fn a_list_with_too_many_buffers_is_refused_whole_and_still_counted_in_and_out() {
        let buffers = vec![(1u32, 0u32, ptr::null_mut::<c_void>()); MAX_BUFFERS + 1];
        let mut input = List::new(&buffers);
        let recorder = Arc::new(Recorder::default());
        // The branch returns before `process`, yet the in-flight count is back to zero.
        assert_eq!(call(&recorder, input.ptr(), ptr::null_mut()), (0, 0));
        assert!(recorder.seen.lock().unwrap().is_empty(), "never truncated");
        // This branch never reaches `process`, yet it ran counted in (it asks for the control).
        assert_eq!(*recorder.counted.lock().unwrap(), [("control", 1)]);
        assert!(recorder.control.bad_buffer_seen());
        assert!(recorder.control.is_disabled());
    }

    #[test]
    fn the_maximum_buffer_count_is_accepted() {
        let buffers = vec![(1u32, 0u32, ptr::null_mut::<c_void>()); MAX_BUFFERS];
        let mut output = List::new(&buffers);
        let recorder = Arc::new(Recorder::default());
        assert_eq!(call(&recorder, ptr::null(), output.ptr()), (0, 0));
        assert_eq!(recorder.seen.lock().unwrap()[0].1.len(), MAX_BUFFERS);
    }

    #[test]
    fn creating_the_hal_makes_no_coreaudio_call() {
        // Construction only builds a dispatch queue and an empty table; dropping it removes
        // nothing because nothing was registered.
        drop(NativeHal::new());
    }

    // -- registration lifecycle through an injected ProcApi ----------------------------------

    /// Records every raw IOProc call, with whether the watched callback was already disabled at
    /// the time, and fails the calls it is told to.
    #[derive(Default)]
    struct TestProcs {
        log: Mutex<Vec<String>>,
        client: Mutex<usize>,
        watch: Mutex<Option<std::sync::Weak<Recorder>>>,
        fail_create: AtomicBool,
        fail_start: AtomicBool,
        fail_destroy: AtomicBool,
    }

    impl TestProcs {
        fn note(&self, what: &str) {
            let watched = self
                .watch
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|w| w.upgrade());
            let disabled = watched.is_some_and(|r| r.control.is_disabled());
            self.log
                .lock()
                .unwrap()
                .push(format!("{what} disabled={disabled}"));
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        fn client(&self) -> *mut c_void {
            *self.client.lock().unwrap() as *mut c_void
        }
    }

    impl ProcApi for TestProcs {
        fn create(
            &self,
            _device: AudioObjectID,
            client: *mut c_void,
        ) -> Result<AudioDeviceIOProc, HalError> {
            self.note("create");
            if self.fail_create.load(Ordering::SeqCst) {
                return Err(HalError::Status(-1));
            }
            *self.client.lock().unwrap() = client as usize;
            Ok(trampoline)
        }

        fn start(&self, _d: AudioObjectID, _p: AudioDeviceIOProc) -> Result<(), HalError> {
            self.note("start");
            if self.fail_start.load(Ordering::SeqCst) {
                return Err(HalError::Status(-2));
            }
            Ok(())
        }

        fn stop(&self, _d: AudioObjectID, _p: AudioDeviceIOProc) -> Result<(), HalError> {
            self.note("stop");
            Ok(())
        }

        fn destroy(&self, _d: AudioObjectID, _p: AudioDeviceIOProc) -> Result<(), HalError> {
            self.note("destroy");
            if self.fail_destroy.load(Ordering::SeqCst) {
                return Err(HalError::Status(-3));
            }
            Ok(())
        }
    }

    fn rig() -> (Arc<TestProcs>, Arc<dyn ProcApi>, Arc<Recorder>) {
        let procs = Arc::new(TestProcs::default());
        let recorder = Arc::new(Recorder::default());
        *procs.watch.lock().unwrap() = Some(Arc::downgrade(&recorder));
        let api: Arc<dyn ProcApi> = procs.clone();
        (procs, api, recorder)
    }

    /// Strong references to the recorder besides the test's own: a registration's, if its state
    /// was kept (leaked) rather than released.
    fn leaked(recorder: &Arc<Recorder>) -> usize {
        Arc::strong_count(recorder) - 1
    }

    /// The registration the proc was created with. Registrations are never freed, so this is
    /// valid for the whole test (and beyond).
    fn registration_of(procs: &TestProcs) -> &'static Registration {
        // SAFETY: `create` recorded the address of a leaked, never-freed `Registration`.
        unsafe { &*procs.client().cast::<Registration>() }
    }

    /// Hygiene for tests that leave state leaked on purpose: release it once it is quiescent.
    fn tidy(registration: &Registration) {
        assert!(registration.drain(Duration::from_secs(1)));
        registration.disable();
        // SAFETY: disabled, and `drain` has just seen no trampoline inside; no proc is live.
        unsafe { registration.release_state() };
    }

    #[test]
    fn a_failed_create_releases_the_state_disabled_and_never_published() {
        let (procs, api, recorder) = rig();
        procs.fail_create.store(true, Ordering::SeqCst);
        assert!(start_registered(&api, 1, recorder.clone()).is_err());
        assert_eq!(procs.log(), ["create disabled=false"]);
        assert!(recorder.control.is_disabled());
        assert_eq!(leaked(&recorder), 0);
    }

    #[test]
    fn a_failed_start_disables_before_it_destroys_and_then_releases_the_state() {
        let (procs, api, recorder) = rig();
        procs.fail_start.store(true, Ordering::SeqCst);
        assert!(start_registered(&api, 1, recorder.clone()).is_err());
        assert_eq!(
            procs.log(),
            [
                "create disabled=false",
                "start disabled=false",
                "destroy disabled=true"
            ]
        );
        assert_eq!(leaked(&recorder), 0, "destroyed and quiescent: released");
    }

    #[test]
    fn a_failed_start_whose_destroy_also_fails_keeps_the_state_disabled_and_silent() {
        let (procs, api, recorder) = rig();
        procs.fail_start.store(true, Ordering::SeqCst);
        procs.fail_destroy.store(true, Ordering::SeqCst);
        assert!(start_registered(&api, 1, recorder.clone()).is_err());
        assert!(recorder.control.is_disabled());
        assert_eq!(leaked(&recorder), 1, "not provably retired: kept");
        // A trampoline that still arrives is silent and never reaches the callback.
        let mut data = [7f32; 4];
        let mut output = List::new(&[(2, 16, data.as_mut_ptr().cast())]);
        // SAFETY: the registration is never freed; the list is valid for the call.
        let status = unsafe { raw_call(procs.client(), ptr::null(), output.ptr()) };
        assert_eq!(status, 0);
        assert_eq!(data, [0.0; 4], "silent");
        assert!(
            recorder.seen.lock().unwrap().is_empty(),
            "callback untouched"
        );
        tidy(registration_of(&procs));
        assert_eq!(leaked(&recorder), 0);
    }

    #[test]
    fn explicit_retirement_disables_first_then_stops_then_destroys() {
        let (procs, api, recorder) = rig();
        let mut session = start_registered(&api, 1, recorder.clone()).unwrap();
        assert!(!recorder.control.is_disabled());
        session.stop().unwrap();
        assert_eq!(
            procs.log()[2..],
            ["stop disabled=true", "destroy disabled=true"]
        );
        drop(session);
        assert_eq!(leaked(&recorder), 0);
    }

    #[test]
    fn dropping_a_started_session_disables_before_stopping_and_destroying() {
        let (procs, api, recorder) = rig();
        let session = start_registered(&api, 1, recorder.clone()).unwrap();
        drop(session);
        assert_eq!(
            procs.log()[2..],
            ["stop disabled=true", "destroy disabled=true"]
        );
        assert_eq!(leaked(&recorder), 0);
    }

    #[test]
    fn a_session_that_cannot_be_destroyed_keeps_its_state_disabled_instead_of_releasing_it() {
        let (procs, api, recorder) = rig();
        let mut session = start_registered(&api, 1, recorder.clone()).unwrap();
        procs.fail_destroy.store(true, Ordering::SeqCst);
        assert!(session.stop().is_err());
        drop(session);
        assert!(recorder.control.is_disabled());
        assert_eq!(leaked(&recorder), 1);
        tidy(registration_of(&procs));
    }

    #[test]
    fn an_enabled_registration_runs_the_callback_and_a_disabled_one_never_does() {
        let (procs, api, recorder) = rig();
        let session = start_registered(&api, 1, recorder.clone()).unwrap();
        // SAFETY: the registration is never freed; no lists.
        let enabled = unsafe { raw_call(procs.client(), ptr::null(), ptr::null_mut()) };
        assert_eq!(enabled, 0);
        assert_eq!(recorder.seen.lock().unwrap().len(), 1);
        drop(session);
        // SAFETY: as above.
        let retired = unsafe { raw_call(procs.client(), ptr::null(), ptr::null_mut()) };
        assert_eq!(retired, 0);
        assert_eq!(
            recorder.seen.lock().unwrap().len(),
            1,
            "retired: not called"
        );
    }

    #[test]
    fn a_trampoline_paused_before_its_increment_never_touches_freed_state_and_returns_silent() {
        let (procs, api, recorder) = rig();
        let observer = Arc::downgrade(&recorder);
        // The registration now owns the only strong reference to the callback state.
        let mut session = start_registered(&api, 1, recorder).unwrap();
        let client = procs.client() as usize;
        // A trampoline that has the client pointer but has not executed its first instruction:
        // it waits here, before `enter()`.
        let (resume, paused) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            paused.recv().unwrap();
            let mut data = [7f32; 8];
            let mut output = List::new(&[(2, 32, data.as_mut_ptr().cast())]);
            // SAFETY: the registration is never freed, which is exactly what is being tested; the
            // list is valid for the call.
            let status = unsafe { raw_call(client as *mut c_void, ptr::null(), output.ptr()) };
            done_tx.send((status, data)).unwrap();
        });
        // Retirement runs to completion while that trampoline is paused: the proc is destroyed,
        // both counters read zero, and the callback state (rings, buffers) is released.
        session.stop().unwrap();
        drop(session);
        assert!(observer.upgrade().is_none(), "the callback state was freed");
        assert_eq!(registration_of(&procs).inflight.load(Ordering::SeqCst), 0);
        // The paused trampoline resumes: it announces itself, finds the registration disabled and
        // returns silent without any access to the freed state.
        resume.send(()).unwrap();
        let (status, data) = done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        thread.join().unwrap();
        assert_eq!(status, 0);
        assert_eq!(data, [0.0; 8], "silent output");
        assert_eq!(registration_of(&procs).inflight.load(Ordering::SeqCst), 0);
        assert!(observer.upgrade().is_none());
    }

    #[test]
    fn retirement_while_a_trampoline_is_counted_in_but_not_yet_in_process_keeps_the_state() {
        let (procs, api, recorder) = rig();
        let mut session = start_registered(&api, 1, recorder.clone()).unwrap();
        let registration = registration_of(&procs);
        // A trampoline that entered (and passed the disabled check) but has not reached
        // `process` (or took the oversized-list branch): counted, though the callback is idle.
        let paused = registration.enter();
        assert!(recorder.control.is_idle());
        session.stop().unwrap();
        let started = Instant::now();
        drop(session);
        assert!(
            started.elapsed() >= DRAIN_WAIT,
            "waited for the drain bound"
        );
        assert_eq!(leaked(&recorder), 1, "kept alive, not released");
        // It resumes against intact state.
        // SAFETY: the state is kept; this entry holder passed the check before retirement.
        assert!(unsafe { registration.callback() }.is_some());
        drop(paused);
        tidy(registration);
        assert_eq!(leaked(&recorder), 0);
    }

    #[test]
    fn retirement_while_a_callback_is_inside_process_keeps_the_state() {
        let (procs, api, recorder) = rig();
        recorder.block.store(true, Ordering::SeqCst);
        let mut session = start_registered(&api, 1, recorder.clone()).unwrap();
        let client = procs.client() as usize;
        let thread = std::thread::spawn(move || {
            // SAFETY: the registration is never freed.
            unsafe { raw_call(client as *mut c_void, ptr::null(), ptr::null_mut()) }
        });
        while !recorder.inside.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        session.stop().unwrap();
        drop(session);
        assert_eq!(leaked(&recorder), 1, "a callback is inside: kept alive");
        recorder.release.store(true, Ordering::SeqCst);
        assert_eq!(thread.join().unwrap(), 0);
        tidy(registration_of(&procs));
    }
}
