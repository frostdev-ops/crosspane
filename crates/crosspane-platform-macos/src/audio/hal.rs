//! The seam between the host's policy and the operating system.
//!
//! [`CoreAudioHost`](super::CoreAudioHost) never calls CoreAudio directly: every HAL fact it reads
//! and every HAL object it creates goes through the [`Hal`] trait. Production uses the native
//! implementation (public `AudioObject*`/`AudioDevice*` calls, see `native.rs`); the tests supply a
//! fake. The trait is deliberately small and read-only: there is **no** method that sets a
//! property, changes a default device, changes a hardware format, or opens an input device other
//! than by an explicit device id the host obtained from an exact UID. That makes "never touch the
//! default input, never change the default output" a property of the type, not of discipline.
//!
//! # Threading
//!
//! Every method except the callbacks is called from the host's single owner thread. Callbacks
//! ([`IoCallback::process`], [`Notifier::notify`]) run on OS threads: the IO thread and the
//! notification queue.

use std::ffi::c_void;
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::Thread;
use std::time::{Duration, Instant};

/// The largest IO cycle the host accepts, in frames. The driver (WP-3.4) supports 0..=4096.
pub const MAX_CALLBACK_FRAMES: usize = 4096;
/// The only sample rate the host serves (WP-3.0b: 48 kHz packed native float32).
pub const RATE: f64 = 48_000.0;

/// `kAudioDeviceClassID` ('adev').
pub const CLASS_AUDIO_DEVICE: u32 = super::ffi::kAudioDeviceClassID;
/// `kAudioDeviceTransportTypeVirtual` ('virt').
pub const TRANSPORT_VIRTUAL: u32 = super::ffi::kAudioDeviceTransportTypeVirtual;
/// `kAudioDeviceTransportTypeBuiltIn` ('bltn').
pub const TRANSPORT_BUILTIN: u32 = super::ffi::kAudioDeviceTransportTypeBuiltIn;
/// `kAudioFormatLinearPCM` ('lpcm').
pub const FORMAT_LINEAR_PCM: u32 = super::ffi::kAudioFormatLinearPCM;
/// `kAudioFormatFlagsNativeFloatPacked`: float, native endian, packed, interleaved.
pub const FLAGS_FLOAT_PACKED: u32 = super::ffi::kAudioFormatFlagsNativeFloatPacked;
/// `kAudioFormatFlagIsNonInterleaved`.
pub const FLAG_NON_INTERLEAVED: u32 = super::ffi::kAudioFormatFlagIsNonInterleaved;

/// An `AudioObjectID` of a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DeviceId(pub u32);

/// An `AudioObjectID` of a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StreamId(pub u32);

/// A registered property listener.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ListenerId(pub u64);

/// A complete `AudioStreamBasicDescription`: every field is compared, not just rate and channels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StreamFormat {
    pub sample_rate: f64,
    pub format_id: u32,
    pub format_flags: u32,
    pub bytes_per_packet: u32,
    pub frames_per_packet: u32,
    pub bytes_per_frame: u32,
    pub channels_per_frame: u32,
    pub bits_per_channel: u32,
    pub reserved: u32,
}

impl StreamFormat {
    /// 48 kHz 32-bit float linear PCM with `channels` channels, interleaved (one buffer of
    /// `channels` channels) or not (`channels` buffers of one channel each).
    pub const fn float32(channels: u32, interleaved: bool) -> Self {
        let bytes_per_frame = if interleaved { 4 * channels } else { 4 };
        Self {
            sample_rate: RATE,
            format_id: FORMAT_LINEAR_PCM,
            format_flags: if interleaved {
                FLAGS_FLOAT_PACKED
            } else {
                FLAGS_FLOAT_PACKED | FLAG_NON_INTERLEAVED
            },
            bytes_per_packet: bytes_per_frame,
            frames_per_packet: 1,
            bytes_per_frame,
            channels_per_frame: channels,
            bits_per_channel: 32,
            reserved: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StreamInfo {
    pub id: StreamId,
    pub format: StreamFormat,
}

/// Everything the host validates about a device, read in one call.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceInfo {
    /// The UID read back from the device (`kAudioDevicePropertyDeviceUID`).
    pub uid: String,
    /// `kAudioObjectPropertyClass`.
    pub class_id: u32,
    /// `kAudioDevicePropertyTransportType`.
    pub transport: u32,
    /// `kAudioDevicePropertyDeviceIsAlive`.
    pub alive: bool,
    /// `kAudioDevicePropertyIsHidden`.
    pub hidden: bool,
    /// `kAudioDevicePropertyNominalSampleRate`.
    pub nominal_rate: f64,
    /// Streams in the input scope, with their virtual (client-facing) formats.
    pub input_streams: Vec<StreamInfo>,
    /// Streams in the output scope, with their virtual (client-facing) formats.
    pub output_streams: Vec<StreamInfo>,
}

/// Why a HAL call failed. Statuses never carry strings from the OS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HalError {
    /// The object (device, stream) no longer exists (`!obj`, `!dev`, `!str`).
    Gone,
    /// The OS refused the operation for lack of permission (`!hog`).
    Denied,
    /// Any other nonzero `OSStatus`.
    Status(i32),
    /// A callback or listener could not be retired in time; its state was kept alive.
    Stuck,
    /// The request could not even be formed (e.g. a UID the OS could not represent).
    Invalid,
}

/// What a listener watches. Each maps to one public `AudioObjectPropertyAddress`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ListenTarget {
    /// `kAudioDevicePropertyDeviceIsRunningSomewhere` of a device: some process runs IO on it.
    DeviceRunning(DeviceId),
    /// `kAudioDevicePropertyDeviceIsAlive`.
    DeviceAlive(DeviceId),
    /// `kAudioDevicePropertyNominalSampleRate`.
    DeviceNominalRate(DeviceId),
    /// `kAudioDevicePropertyStreamConfiguration` in the output scope.
    DeviceOutputConfig(DeviceId),
    /// `kAudioStreamPropertyVirtualFormat` of a stream.
    StreamFormat(StreamId),
    /// `kAudioHardwarePropertyDefaultOutputDevice` of the system object.
    DefaultOutput,
    /// `kAudioHardwarePropertyDefaultSystemOutputDevice` of the system object.
    DefaultSystemOutput,
    /// `kAudioHardwarePropertyDevices` of the system object.
    DeviceList,
    /// `kAudioHardwarePropertyServiceRestarted` of the system object.
    ServiceRestarted,
}

/// One buffer of an IO cycle: the same fields as the SDK's `AudioBuffer`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct IoBuffer {
    /// `mNumberChannels`: channels interleaved in this buffer.
    pub channels: u32,
    /// `mDataByteSize`.
    pub byte_size: u32,
    /// `mData`: null for a disabled stream.
    pub data: *mut c_void,
}

/// Admission state shared between the owner thread and one IO callback.
///
/// The callback calls [`enter`](Self::enter) first and holds the [`Admission`] for the whole call;
/// the owner calls [`disable`](Self::disable) and then [`drain`](Self::drain) before it frees or
/// recycles anything the callback touches. All operations are lock-free atomics (sequentially
/// consistent, so "disabled" and "a callback is inside" can never both be missed).
#[derive(Debug, Default)]
pub struct CallbackControl {
    disabled: AtomicBool,
    in_flight: AtomicU32,
    gate_closed: AtomicBool,
    bad_buffer: AtomicBool,
    callbacks: AtomicU64,
    frames_moved: AtomicU64,
    frames_dropped: AtomicU64,
}

/// Counters for diagnostics and tests; sample data never appears in them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallbackStats {
    pub callbacks: u64,
    pub frames_moved: u64,
    pub frames_dropped: u64,
}

/// Proof that the holder is the only callback currently inside; dropping it leaves.
#[derive(Debug)]
pub struct Admission<'a>(&'a CallbackControl);

impl Drop for Admission<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl CallbackControl {
    /// Enter a callback. `None` means another invocation is already inside (never the case for a
    /// well-behaved HAL); the caller does nothing. Succeeds while disabled so output callbacks can
    /// still write silence.
    pub fn enter(&self) -> Option<Admission<'_>> {
        if self.in_flight.fetch_add(1, Ordering::SeqCst) != 0 {
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Admission(self))
    }

    /// Stop all production/consumption, effective for any callback that has not yet checked.
    pub fn disable(&self) {
        self.disabled.store(true, Ordering::SeqCst);
    }

    pub fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::SeqCst)
    }

    /// True when no callback is inside.
    pub fn is_idle(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst) == 0
    }

    /// Wait (bounded) until no callback is inside. Call after [`disable`](Self::disable).
    pub fn drain(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while !self.is_idle() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
        true
    }

    /// The callback observed the I/O gate closed: it latches, disabling this registration at once
    /// and for good (reopening the gate does not undo it).
    pub fn latch_gate_closed(&self) {
        self.gate_closed.store(true, Ordering::SeqCst);
        self.disable();
    }

    pub fn gate_closed_seen(&self) -> bool {
        self.gate_closed.load(Ordering::SeqCst)
    }

    /// The callback saw a buffer that does not match the validated layout; it also disables.
    pub fn mark_bad(&self) {
        self.bad_buffer.store(true, Ordering::SeqCst);
        self.disable();
    }

    pub fn bad_buffer_seen(&self) -> bool {
        self.bad_buffer.load(Ordering::SeqCst)
    }

    pub fn note_cycle(&self, moved: usize, dropped: usize) {
        self.callbacks.fetch_add(1, Ordering::Relaxed);
        self.frames_moved.fetch_add(moved as u64, Ordering::Relaxed);
        self.frames_dropped
            .fetch_add(dropped as u64, Ordering::Relaxed);
    }

    pub fn stats(&self) -> CallbackStats {
        CallbackStats {
            callbacks: self.callbacks.load(Ordering::Relaxed),
            frames_moved: self.frames_moved.load(Ordering::Relaxed),
            frames_dropped: self.frames_dropped.load(Ordering::Relaxed),
        }
    }
}

/// The code the HAL runs on its IO thread for one `AudioDeviceIOProc` registration.
pub trait IoCallback: Send + Sync + Debug {
    /// Process one IO cycle. Allocates nothing, takes no locks, never blocks, performs no HAL
    /// call, retains no pointer past the return.
    ///
    /// # Safety
    ///
    /// Every buffer's `data` pointer is null or valid for `byte_size` bytes (reads for input
    /// buffers, reads and writes for output buffers) for the duration of the call, and nothing else
    /// accesses that memory meanwhile.
    unsafe fn process(&self, input: &[IoBuffer], output: &[IoBuffer]);

    /// The admission state the owner uses to retire this callback.
    fn control(&self) -> &CallbackControl;
}

/// A started IOProc registration. `stop` is idempotent.
///
/// Dropping the session retires it: disable, destroy, wait (bounded) for callbacks inside, then
/// release the callback state. The native registration shell the HAL was given is never freed,
/// and the callback state is released only when quiescence is proven; the host `mem::forget`s a
/// session whose retirement did not complete.
pub trait IoSession: Send + Debug {
    /// `AudioDeviceStop` then `AudioDeviceDestroyIOProcID`, after disabling the callback. After
    /// `Ok`, no new callback does anything: a trampoline that starts later finds the registration
    /// disabled and returns silent without touching the callback state.
    fn stop(&mut self) -> Result<(), HalError>;
}

/// A change-notification flag: the notification thread sets it and wakes the owner thread, the
/// owner reads the state from the HAL and reconciles. Callbacks post nothing else.
#[derive(Debug)]
pub struct Notifier {
    dirty: AtomicBool,
    retired: AtomicBool,
    wake: Thread,
}

impl Notifier {
    pub fn new(wake: Thread) -> Arc<Self> {
        Arc::new(Self {
            dirty: AtomicBool::new(false),
            retired: AtomicBool::new(false),
            wake,
        })
    }

    /// Called by the HAL's notification thread. Lock-free and allocation-free.
    pub fn notify(&self) {
        if !self.retired.load(Ordering::SeqCst) {
            self.dirty.store(true, Ordering::SeqCst);
            self.wake.unpark();
        }
    }

    /// Owner thread: was a notification posted since the last call?
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::SeqCst)
    }

    /// Owner thread: mark for another reconcile pass.
    pub fn mark_dirty(&self) {
        if !self.retired.load(Ordering::SeqCst) {
            self.dirty.store(true, Ordering::SeqCst);
        }
    }

    /// Owner thread: from now on notifications have no effect.
    pub fn retire(&self) {
        self.retired.store(true, Ordering::SeqCst);
        self.dirty.store(false, Ordering::SeqCst);
    }

    pub fn is_retired(&self) -> bool {
        self.retired.load(Ordering::SeqCst)
    }
}

/// The operating system, as the host sees it. See the module documentation.
pub trait Hal: Send + Sync + 'static {
    /// `kAudioHardwarePropertyTranslateUIDToDevice` with a CFString qualifier. `Ok(None)` when no
    /// device has that UID (for the fixed Crosspane UIDs: the plug-in is not installed).
    fn translate_uid(&self, uid: &str) -> Result<Option<DeviceId>, HalError>;

    /// Read the validated properties of one device.
    fn device_info(&self, device: DeviceId) -> Result<DeviceInfo, HalError>;

    fn is_alive(&self, device: DeviceId) -> Result<bool, HalError>;

    /// `kAudioDevicePropertyDeviceIsRunningSomewhere`.
    fn is_running_somewhere(&self, device: DeviceId) -> Result<bool, HalError>;

    /// The system default output device, `Ok(None)` when there is none.
    fn default_output_device(&self) -> Result<Option<DeviceId>, HalError>;

    /// The default system output, distinct from the default output. A stub without that device
    /// supplies no candidate; the native implementation reads the public system property.
    fn default_system_output_device(&self) -> Result<Option<DeviceId>, HalError> {
        Ok(None)
    }

    /// Device ids in the order returned by `kAudioHardwarePropertyDevices`, without changing any
    /// device. Existing stubs have no inventory beyond their explicit default.
    fn device_ids(&self) -> Result<Vec<DeviceId>, HalError> {
        Ok(Vec::new())
    }

    /// Register a change listener. Notifications call [`Notifier::notify`] on an OS thread.
    fn add_listener(
        &self,
        target: ListenTarget,
        notifier: Arc<Notifier>,
    ) -> Result<ListenerId, HalError>;

    /// Unregister a listener.
    fn remove_listener(&self, listener: ListenerId) -> Result<(), HalError>;

    /// Wait (bounded) until every notification already dispatched has finished running. `false`
    /// when one is still running after `timeout`.
    fn flush_listeners(&self, timeout: Duration) -> bool;

    /// `AudioDeviceCreateIOProcID` + `AudioDeviceStart` on exactly `device`.
    fn start_io(
        &self,
        device: DeviceId,
        callback: Arc<dyn IoCallback>,
    ) -> Result<Box<dyn IoSession>, HalError>;
}
