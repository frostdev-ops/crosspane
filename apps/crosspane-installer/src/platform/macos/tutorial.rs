//! Own-window facts and an admitted private child. No local fact establishes installer readiness.
use super::native_io::AdmittedTutorialChild;
use crate::fixture::{
    FixtureClock, FixtureError, InheritedFixtureChild, OwnToneState, OwnWindowFacts,
    PipeFixturePort, SpeakersSelection, ToneId,
};
use crate::tutorial_window::{TutorialNative, WindowObservation};
use crosspane_types::id::WindowId;
use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
use objc2_core_foundation::{CFDictionary, CFNumber, CFString, CFType};
use objc2_core_graphics::{
    CGWindowListCopyWindowInfo, CGWindowListOption, kCGNullWindowID, kCGWindowOwnerPID,
};
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

/// No title is exposed by Debug. Only the current child's own AppKit namespace is queried.
pub struct OwnWindow {
    pub pid: u32,
    pub number: i64,
    pub title: String,
    pub primary: bool,
}
impl std::fmt::Debug for OwnWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OwnWindow")
    }
}
pub trait OwnWindowObserver {
    /// Nonblocking, fresh own surfaces; None is pending. Foreign PID rows are never admitted.
    fn windows(&mut self) -> Option<Result<Vec<OwnWindow>, FixtureError>>;
}
struct AppKitWindows;
impl OwnWindowObserver for AppKitWindows {
    fn windows(&mut self) -> Option<Result<Vec<OwnWindow>, FixtureError>> {
        Some((|| {
            let main = MainThreadMarker::new().ok_or(FixtureError::Unavailable)?;
            let windows = NSApplication::sharedApplication(main).windows();
            if windows.len() > 64 {
                return Err(FixtureError::Unavailable);
            }
            Ok(windows
                .iter()
                .map(|w| OwnWindow {
                    pid: std::process::id(),
                    number: w.windowNumber() as i64,
                    title: w.title().to_string(),
                    primary: w.canBecomeMainWindow(),
                })
                .collect())
        })())
    }
}
pub struct MacTutorial {
    pid: u32,
    observer: Box<dyn OwnWindowObserver>,
    window: Option<WindowId>,
    last_call: u64,
    output: Option<OutputController>,
}
impl std::fmt::Debug for MacTutorial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MacTutorial")
    }
}
impl MacTutorial {
    /// Construction performs no AppKit/device operation; observation occurs on the GUI thread.
    pub fn new() -> Self {
        Self::with_output(
            std::process::id(),
            Box::new(AppKitWindows),
            Arc::new(CoreAudio),
        )
    }
    pub fn with_observer(pid: u32, observer: Box<dyn OwnWindowObserver>) -> Self {
        Self {
            pid,
            observer,
            window: None,
            last_call: 0,
            output: None,
        }
    }
    /// Detached output injection; native construction remains inert until a validated PlayTone.
    pub fn with_output(
        pid: u32,
        observer: Box<dyn OwnWindowObserver>,
        hal: Arc<dyn TutorialHal>,
    ) -> Self {
        let mut this = Self::with_observer(pid, observer);
        this.output = Some(OutputController {
            hal,
            peer: None,
            last: 0,
            job: None,
        });
        this
    }
}
impl Default for MacTutorial {
    fn default() -> Self {
        Self::new()
    }
}
impl TutorialNative for MacTutorial {
    fn observe_window(&mut self, call: u64, title: &str) -> Option<WindowObservation> {
        if self.pid == 0
            || call == 0
            || call < self.last_call
            || title.len() > 160
            || !title.starts_with("Crosspane practice | ")
            || title.chars().any(char::is_control)
        {
            return Some(Err(FixtureError::BadCall));
        }
        self.last_call = call;
        let rows = self.observer.windows()?;
        Some(rows.and_then(|rows| {
            if rows.len() > 64 {
                return Err(FixtureError::Unavailable);
            }
            let mut matching = rows
                .iter()
                .filter(|w| w.pid == self.pid && w.primary && w.title == title);
            let row = matching.next().ok_or(FixtureError::UnknownWindow)?;
            if matching.next().is_some() {
                return Err(FixtureError::AmbiguousWindow);
            }
            let number = u32::try_from(row.number)
                .ok()
                .filter(|n| *n > 0)
                .ok_or(FixtureError::UnknownWindow)?;
            let id = WindowId(u64::from(number));
            if self.window.is_some_and(|pinned| pinned != id) {
                return Err(FixtureError::UnknownWindow);
            }
            self.window = Some(id);
            // AppKit presence proves neither the user's workspace nor the initial display.
            Ok((id, OwnWindowFacts::Unknown))
        }))
    }
    fn play_tone(
        &mut self,
        tone: ToneId,
        output: &SpeakersSelection,
    ) -> Option<Result<(), FixtureError>> {
        self.output
            .as_mut()
            .map_or(Some(Err(FixtureError::Unavailable)), |o| {
                o.play(tone, output)
            })
    }
    fn stop_tone(&mut self, tone: ToneId) -> Option<Result<(), FixtureError>> {
        self.output
            .as_mut()
            .map_or(Some(Err(FixtureError::Unavailable)), |o| o.stop(tone))
    }
    fn tone_state(&self) -> OwnToneState {
        self.output
            .as_ref()
            .map_or(OwnToneState::Stopped, OutputController::state)
    }
}

pub trait ChildWindowProbe: Send + Sync {
    /// Fresh PID-only public metadata; no title, capture, AX, or permission request.
    fn absent(&self, pid: u32) -> Result<bool, FixtureError>;
}
struct QuartzAbsence;
impl ChildWindowProbe for QuartzAbsence {
    fn absent(&self, pid: u32) -> Result<bool, FixtureError> {
        let rows = CGWindowListCopyWindowInfo(
            CGWindowListOption::OptionAll | CGWindowListOption::ExcludeDesktopElements,
            kCGNullWindowID,
        )
        .ok_or(FixtureError::Unavailable)?;
        // SAFETY: the public function returns an immutable CFArray of CFDictionary objects.
        let rows = unsafe { rows.cast_unchecked::<CFType>() };
        if rows.len() > 4096 {
            return Err(FixtureError::Unavailable);
        }
        for row in rows.iter() {
            let dictionary = row
                .downcast::<CFDictionary>()
                .map_err(|_| FixtureError::Unavailable)?;
            // SAFETY: Quartz dictionaries have CFString keys; each CFType value is checked below.
            let dictionary = unsafe { dictionary.cast_unchecked::<CFString, CFType>() };
            // SAFETY: immutable public CoreGraphics property key, retained by the framework.
            let key = unsafe { kCGWindowOwnerPID };
            let owner = dictionary
                .get(key)
                .and_then(|v| v.downcast::<CFNumber>().ok())
                .and_then(|v| v.as_i64())
                .ok_or(FixtureError::Unavailable)?;
            if owner == i64::from(pid) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
static ABSENCE_QUERIES: AtomicUsize = AtomicUsize::new(0);
struct AbsenceLease;
impl Drop for AbsenceLease {
    fn drop(&mut self) {
        ABSENCE_QUERIES.fetch_sub(1, Ordering::Release);
    }
}
struct FixtureChild {
    child: AdmittedTutorialChild,
    probe: Arc<dyn ChildWindowProbe>,
    pending: Option<mpsc::Receiver<Result<bool, FixtureError>>>,
    eof: Option<Instant>,
    confirmed: Option<bool>,
}
impl InheritedFixtureChild for FixtureChild {
    fn pid(&self) -> u32 {
        self.child.identity().pid
    }
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let n = self.child.read(bytes)?;
        if n != 0 {
            return Ok(n);
        }
        self.eof.get_or_insert_with(Instant::now);
        let _ = self.cleanup_confirmed();
        // Lead ruling: actual EOF + reap permits one pending absence query, at most500ms.
        // The common port's original call deadline still runs; this method never waits.
        if self.pending.is_some() && self.confirmed.is_none() {
            Err(io::ErrorKind::WouldBlock.into())
        } else {
            Ok(0)
        }
    }
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.child.write(bytes)
    }
    fn cleanup_confirmed(&mut self) -> Result<bool, FixtureError> {
        if let Some(confirmed) = self.confirmed {
            return Ok(confirmed);
        }
        let Some(eof) = self.eof else {
            return Ok(false);
        };
        if eof.elapsed() >= Duration::from_millis(500) {
            self.confirmed = Some(false);
            self.pending = None;
            return Ok(false);
        }
        if !self
            .child
            .reaped()
            .map_err(|_| FixtureError::CleanupFailed)?
        {
            return Ok(false);
        }
        if let Some(pending) = &self.pending {
            return match pending.try_recv() {
                Ok(result) => {
                    self.pending = None;
                    let confirmed = result.unwrap_or(false);
                    self.confirmed = Some(confirmed);
                    Ok(confirmed)
                }
                Err(mpsc::TryRecvError::Empty) => Ok(false),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.confirmed = Some(false);
                    self.pending = None;
                    Ok(false)
                }
            };
        }
        // Latch refusal before starting: failure/present/expiry never restarts the query.
        self.confirmed = Some(false);
        ABSENCE_QUERIES
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 4).then_some(n + 1)
            })
            .map_err(|_| FixtureError::Busy)?;
        let lease = AbsenceLease;
        let (send, receive) = mpsc::sync_channel(1);
        let (probe, pid) = (self.probe.clone(), self.pid());
        thread::Builder::new()
            .name("tutorial-window-absence".into())
            .spawn(move || {
                let _lease = lease;
                let _ = send.send(probe.absent(pid));
            })
            .map_err(|_| FixtureError::CleanupFailed)?;
        self.pending = Some(receive);
        self.confirmed = None;
        Ok(false)
    }
    fn retire(&mut self) {
        self.child.retire();
    }
}
fn port(
    child: AdmittedTutorialChild,
    probe: Arc<dyn ChildWindowProbe>,
    clock: FixtureClock,
) -> Result<PipeFixturePort, FixtureError> {
    PipeFixturePort::new(
        Box::new(FixtureChild {
            child,
            probe,
            pending: None,
            eof: None,
            confirmed: None,
        }),
        clock,
    )
}
pub fn fixture_port(
    child: AdmittedTutorialChild,
    clock: FixtureClock,
) -> Result<PipeFixturePort, FixtureError> {
    port(child, Arc::new(QuartzAbsence), clock)
}
/// An injected absence answer is restricted to a native-admitted scratch child.
pub fn fixture_port_with(
    child: AdmittedTutorialChild,
    probe: Arc<dyn ChildWindowProbe>,
    clock: FixtureClock,
) -> Result<PipeFixturePort, FixtureError> {
    if child.source() != crate::agent_contract::ObservationSource::Demo {
        return Err(FixtureError::NotOwned);
    }
    port(child, probe, clock)
}

// Output-only CoreAudio. The trusted inherited parent pins the first valid peer for this child.
mod ffi;
use crosspane_types::id::NodeId;
pub use ffi::AudioStreamBasicDescription as OutputFormat;
use ffi::*;
use objc2_core_foundation::CFRetained;
use std::{cell::UnsafeCell, ffi::c_void, mem::size_of, ptr::NonNull};
pub const SPEAKERS_APP_UID: &str = "io.frostdev.crosspane.audio.v0.speakers.app";
const FRAMES: usize = 96_000;
const MAX_OUTPUT_BYTES: usize = 32_768;
const MAX_OUTPUT_BUFFERS: usize = 16;
static OUTPUT_WORKERS: AtomicUsize = AtomicUsize::new(0);
static REGISTRATIONS: AtomicUsize = AtomicUsize::new(0);
#[derive(Clone)]
pub struct OutputDevice {
    pub id: u32,
    pub uid: String,
    pub alive: bool,
    pub hidden: bool,
    pub class: u32,
    pub transport: u32,
    pub nominal_rate: f64,
    pub inputs: Vec<u32>,
    pub outputs: Vec<(u32, OutputFormat)>,
}
impl std::fmt::Debug for OutputDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OutputDevice")
    }
}
impl OutputDevice {
    fn admit(&self) -> Result<(), FixtureError> {
        if self.id == 0
            || self.uid != SPEAKERS_APP_UID
            || !self.alive
            || self.hidden
            || self.class != kAudioDeviceClassID
            || self.transport != kAudioDeviceTransportTypeVirtual
            || !self.inputs.is_empty()
            || self.outputs.len() != 1
            || self.outputs[0].0 == 0
        {
            return Err(FixtureError::OutputUnavailable);
        }
        let f = self.outputs[0].1;
        if self.nominal_rate != 48_000.0
            || f != (OutputFormat {
                mSampleRate: 48_000.0,
                mFormatID: kAudioFormatLinearPCM,
                mFormatFlags: kAudioFormatFlagsNativeFloatPacked,
                mBytesPerPacket: 8,
                mFramesPerPacket: 1,
                mBytesPerFrame: 8,
                mChannelsPerFrame: 2,
                mBitsPerChannel: 32,
                mReserved: 0,
            })
        {
            return Err(FixtureError::UnsupportedFormat);
        }
        Ok(())
    }
    fn same(&self, other: &Self) -> bool {
        other.admit().is_ok() && self.id == other.id && self.outputs == other.outputs
    }
}
pub trait ToneSession: Send {
    fn start(&mut self) -> Result<(), FixtureError>;
    fn stop(&mut self) -> Result<(), FixtureError>;
    /// Success requires destruction plus callback quiescence; uncertainty retains all state.
    fn destroy(&mut self) -> Result<(), FixtureError>;
}
pub trait TutorialHal: Send + Sync {
    fn inspect(&self) -> Result<OutputDevice, FixtureError>;
    /// CleanupFailed may retain a registration; its worker lease must then remain occupied.
    fn create(
        &self,
        device: u32,
        buffer: Arc<ToneBuffer>,
    ) -> Result<Box<dyn ToneSession>, FixtureError>;
}
pub struct ToneBuffer {
    pcm: Box<[f32]>,
    enabled: std::sync::atomic::AtomicU8,
    finished: std::sync::atomic::AtomicBool,
    bad: std::sync::atomic::AtomicBool,
    cursor: AtomicUsize,
}
impl std::fmt::Debug for ToneBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ToneBuffer")
    }
}
impl ToneBuffer {
    fn new() -> Self {
        Self {
            pcm: (0..FRAMES)
                .map(|i| {
                    let ramp = (i.min(FRAMES - 1 - i) as f64 / 960.0).min(1.0);
                    ((std::f64::consts::TAU * 1000.0 * i as f64 / 48_000.0).sin()
                        * 0.0316228
                        * ramp) as f32
                })
                .collect(),
            enabled: std::sync::atomic::AtomicU8::new(1),
            finished: false.into(),
            bad: false.into(),
            cursor: AtomicUsize::new(0),
        }
    }
    fn disable(&self) {
        self.enabled.store(0, Ordering::SeqCst);
    }
    /// Safe fake-output seam, also used by the admitted native interleaved callback.
    pub fn render(&self, channels: u32, samples: &mut [f32]) {
        samples.fill(0.0);
        if channels != 2
            || samples.is_empty()
            || !samples.len().is_multiple_of(2)
            || samples.len() > MAX_OUTPUT_BYTES / 4
        {
            self.bad.store(true, Ordering::Release);
            self.disable();
            return;
        }
        if self.enabled.load(Ordering::SeqCst) != 2 {
            return;
        }
        let frames = samples.len() / 2;
        let start = self
            .cursor
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < FRAMES).then_some(n.saturating_add(frames).min(FRAMES))
            })
            .unwrap_or(FRAMES);
        let (output_frames, _) = samples.as_chunks_mut::<2>();
        for (i, out) in output_frames.iter_mut().enumerate() {
            let sample = self.pcm.get(start + i).copied().unwrap_or(0.0);
            out.fill(sample);
        }
        if start.saturating_add(frames) >= FRAMES {
            self.finished.store(true, Ordering::Release);
            self.disable();
        }
    }
}
struct Registration {
    entries: AtomicUsize,
    disabled: std::sync::atomic::AtomicBool,
    buffer: UnsafeCell<Option<Arc<ToneBuffer>>>,
}
// SAFETY: buffer is read only after entry announcement and disabled check; release occurs only
// after destroy and an idle observation. Late entries see disabled and never access buffer.
unsafe impl Sync for Registration {}
struct CallbackEntry<'a>(&'a Registration);
impl Drop for CallbackEntry<'_> {
    fn drop(&mut self) {
        self.0.entries.fetch_sub(1, Ordering::SeqCst);
    }
}
fn output_header(address: usize) -> bool {
    address != 0 && address.is_multiple_of(std::mem::align_of::<AudioBufferList>())
}
fn output_range(address: usize, bytes: usize) -> bool {
    address != 0 && address.checked_add(bytes).is_some()
}
unsafe extern "C" fn output_callback(
    _: u32,
    _: *const c_void,
    _: *const AudioBufferList,
    _: *const c_void,
    output: *mut AudioBufferList,
    _: *const c_void,
    client: *mut c_void,
) -> i32 {
    if client.is_null() {
        return 0;
    }
    // SAFETY: client refers to a permanently retained Registration, independent of the header.
    let registration = unsafe { &*client.cast::<Registration>() };
    registration.entries.fetch_add(1, Ordering::SeqCst);
    let _entry = CallbackEntry(registration);
    // SAFETY: after this check retirement cannot release buffer until this entry leaves.
    let buffer = if registration.disabled.load(Ordering::SeqCst) {
        None
    } else {
        // SAFETY: this entered callback prevents release until its guard leaves.
        unsafe { (&*registration.buffer.get()).as_ref() }
    };
    if !output_header(output.addr()) {
        if let Some(buffer) = buffer {
            buffer.bad.store(true, Ordering::Release);
            buffer.disable();
        }
        return 0;
    }
    // SAFETY: output is the HAL's live output header, never input or timestamps.
    let count = unsafe { (*output).mNumberBuffers } as usize;
    // SAFETY: the flexible array starts at mBuffers; HAL guarantees count readable descriptors.
    let first = unsafe { std::ptr::addr_of!((*output).mBuffers) }.cast::<AudioBuffer>();
    let mut valid = count == 1;
    for i in 0..count.min(MAX_OUTPUT_BUFFERS) {
        // SAFETY: i < count; the HAL owns each descriptor and its writable data extent.
        let descriptor = unsafe { first.add(i).read() };
        let bytes = descriptor.mDataByteSize as usize;
        let extent = output_range(descriptor.mData.addr(), bytes);
        let admitted = extent
            && bytes <= MAX_OUTPUT_BYTES
            && bytes > 0
            && bytes.is_multiple_of(8)
            && descriptor.mData.addr().is_multiple_of(4)
            && descriptor.mNumberChannels == 2
            && count == 1;
        valid &= admitted;
        if admitted && let Some(buffer) = buffer {
            // SAFETY: validated writable/aligned float32 stereo extent; output only, no aliases.
            let samples = unsafe {
                std::slice::from_raw_parts_mut(descriptor.mData.cast::<f32>(), bytes / 4)
            };
            buffer.render(2, samples);
        } else if extent {
            // SAFETY: HAL data is writable for bytes; the zero write is bounded even for bad layout.
            unsafe {
                std::ptr::write_bytes(
                    descriptor.mData.cast::<u8>(),
                    0,
                    bytes.min(MAX_OUTPUT_BYTES),
                )
            };
        }
    }
    if !valid && let Some(buffer) = buffer {
        buffer.bad.store(true, Ordering::Release);
        buffer.disable();
    }
    0
}
struct CoreAudio;
fn property<T: Copy + Default>(device: u32, selector: u32, scope: u32) -> Result<T, FixtureError> {
    let address = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: 0,
    };
    let mut value = T::default();
    let mut bytes = size_of::<T>() as u32;
    // SAFETY: fixed writable scalar and matching size; no qualifier or setter.
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            &address,
            0,
            std::ptr::null(),
            &mut bytes,
            std::ptr::from_mut(&mut value).cast(),
        )
    };
    if status != 0 || bytes as usize != size_of::<T>() {
        return Err(FixtureError::OutputUnavailable);
    }
    Ok(value)
}
fn stream_ids(device: u32, scope: u32) -> Result<Vec<u32>, FixtureError> {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyStreams,
        mScope: scope,
        mElement: 0,
    };
    let mut bytes = 0;
    // SAFETY: valid property address and writable size scalar; no qualifier.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(device, &address, 0, std::ptr::null(), &mut bytes)
    };
    if status != 0 || !bytes.is_multiple_of(4) || bytes > 64 * 4 {
        return Err(FixtureError::OutputUnavailable);
    }
    if bytes == 0 {
        return Ok(Vec::new());
    }
    let mut ids = vec![0; bytes as usize / 4];
    let expected = bytes;
    // SAFETY: ids has exactly bytes writable bytes; size readback is checked without truncation.
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            &address,
            0,
            std::ptr::null(),
            &mut bytes,
            ids.as_mut_ptr().cast(),
        )
    };
    if status != 0 || bytes != expected {
        return Err(FixtureError::OutputChanged);
    }
    Ok(ids)
}
impl TutorialHal for CoreAudio {
    fn inspect(&self) -> Result<OutputDevice, FixtureError> {
        let uid = CFString::from_str(SPEAKERS_APP_UID);
        let qualifier = std::ptr::from_ref(&*uid);
        let address = AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyTranslateUIDToDevice,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: 0,
        };
        let mut id = 0u32;
        let mut bytes = 4;
        // SAFETY: retained fixed UID qualifier, writable device-id slot and exact qualifier size.
        let status = unsafe {
            AudioObjectGetPropertyData(
                kAudioObjectSystemObject,
                &address,
                size_of::<*const CFString>() as u32,
                std::ptr::from_ref(&qualifier).cast(),
                &mut bytes,
                std::ptr::from_mut(&mut id).cast(),
            )
        };
        if status != 0 || bytes != 4 || id == 0 {
            return Err(FixtureError::OutputUnavailable);
        }
        let raw: *const CFString = property(
            id,
            kAudioDevicePropertyDeviceUID,
            kAudioObjectPropertyScopeGlobal,
        )?;
        let raw = NonNull::new(raw.cast_mut()).ok_or(FixtureError::OutputUnavailable)?;
        // SAFETY: DeviceUID returns a caller-owned retained CFString, released on this scope's exit.
        let actual = unsafe { CFRetained::from_raw(raw) };
        if actual.length() != SPEAKERS_APP_UID.len() as isize {
            return Err(FixtureError::OutputUnavailable);
        }
        let global = kAudioObjectPropertyScopeGlobal;
        let outputs = stream_ids(id, kAudioObjectPropertyScopeOutput)?
            .into_iter()
            .map(|stream| {
                property(stream, kAudioStreamPropertyVirtualFormat, global)
                    .map(|format| (stream, format))
            })
            .collect::<Result<_, _>>()?;
        Ok(OutputDevice {
            id,
            uid: actual.to_string(),
            alive: property::<u32>(id, kAudioDevicePropertyDeviceIsAlive, global)? == 1,
            hidden: property::<u32>(id, kAudioDevicePropertyIsHidden, global)? != 0,
            class: property(id, kAudioObjectPropertyClass, global)?,
            transport: property(id, kAudioDevicePropertyTransportType, global)?,
            nominal_rate: property(id, kAudioDevicePropertyNominalSampleRate, global)?,
            inputs: stream_ids(id, kAudioObjectPropertyScopeInput)?,
            outputs,
        })
    }
    fn create(
        &self,
        device: u32,
        buffer: Arc<ToneBuffer>,
    ) -> Result<Box<dyn ToneSession>, FixtureError> {
        // At most 32 small permanent callback tombstones per child; uncertain large state occupies
        // one of four worker leases permanently, so no unbounded retry can leak callback buffers.
        REGISTRATIONS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 32).then_some(n + 1)
            })
            .map_err(|_| FixtureError::Busy)?;
        let registration = Box::leak(Box::new(Registration {
            entries: AtomicUsize::new(0),
            disabled: false.into(),
            buffer: UnsafeCell::new(Some(buffer)),
        }));
        let mut proc_id = None;
        // SAFETY: signature matches the approved SDK; registration never freed, output-only callback.
        let status = unsafe {
            AudioDeviceCreateIOProcID(
                device,
                output_callback,
                std::ptr::from_ref(registration).cast_mut().cast(),
                &mut proc_id,
            )
        };
        if status != 0 || proc_id.is_none() {
            registration.disabled.store(true, Ordering::SeqCst);
            return Err(FixtureError::CleanupFailed);
        }
        Ok(Box::new(NativeTone {
            device,
            proc_id: proc_id.ok_or(FixtureError::CleanupFailed)?,
            registration,
        }))
    }
}
struct NativeTone {
    device: u32,
    proc_id: AudioDeviceIOProc,
    registration: &'static Registration,
}
impl ToneSession for NativeTone {
    fn start(&mut self) -> Result<(), FixtureError> {
        // SAFETY: this IOProc belongs only to this admitted device and has not been destroyed.
        if unsafe { AudioDeviceStart(self.device, self.proc_id) } == 0 {
            Ok(())
        } else {
            Err(FixtureError::OutputUnavailable)
        }
    }
    fn stop(&mut self) -> Result<(), FixtureError> {
        self.registration.disabled.store(true, Ordering::SeqCst);
        // SAFETY: this worker owns its live IOProc; generation has already been disabled.
        if unsafe { AudioDeviceStop(self.device, self.proc_id) } == 0 {
            Ok(())
        } else {
            Err(FixtureError::CleanupFailed)
        }
    }
    fn destroy(&mut self) -> Result<(), FixtureError> {
        self.registration.disabled.store(true, Ordering::SeqCst);
        // SAFETY: own created IOProc only; tombstone survives arbitrarily late callbacks.
        if unsafe { AudioDeviceDestroyIOProcID(self.device, self.proc_id) } != 0 {
            return Err(FixtureError::CleanupFailed);
        }
        let deadline = Instant::now() + Duration::from_millis(50);
        while self.registration.entries.load(Ordering::SeqCst) != 0 {
            if Instant::now() >= deadline {
                return Err(FixtureError::CleanupFailed);
            }
            thread::sleep(Duration::from_micros(100));
        }
        // SAFETY: disabled, destroyed, observed idle after disable; late callbacks never read buffer.
        unsafe {
            (*self.registration.buffer.get()).take();
        }
        Ok(())
    }
}
struct OutputLease;
impl Drop for OutputLease {
    fn drop(&mut self) {
        OUTPUT_WORKERS.fetch_sub(1, Ordering::Release);
    }
}
const OPENING: u8 = 0;
const RUNNING: u8 = 1;
const STOPPED: u8 = 2;
fn result_code(error: FixtureError) -> u8 {
    match error {
        FixtureError::UnsupportedFormat => 3,
        FixtureError::OutputChanged => 4,
        FixtureError::CleanupFailed => 5,
        _ => 6,
    }
}
fn code_error(code: u8) -> FixtureError {
    match code {
        3 => FixtureError::UnsupportedFormat,
        4 => FixtureError::OutputChanged,
        5 => FixtureError::CleanupFailed,
        _ => FixtureError::OutputUnavailable,
    }
}
struct ToneJob {
    tone: ToneId,
    buffer: Arc<ToneBuffer>,
    state: Arc<std::sync::atomic::AtomicU8>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    failed: Arc<std::sync::atomic::AtomicBool>,
    opened: Instant,
    stopping: Arc<std::sync::OnceLock<Instant>>,
}
struct OutputController {
    hal: Arc<dyn TutorialHal>,
    peer: Option<NodeId>,
    last: u64,
    job: Option<ToneJob>,
}
impl OutputController {
    fn play(
        &mut self,
        tone: ToneId,
        output: &SpeakersSelection,
    ) -> Option<Result<(), FixtureError>> {
        if tone.0 == 0
            || output.device_key != format!("crosspane.{}.speaker", output.peer)
            || self.peer.is_some_and(|p| p != output.peer)
        {
            return Some(Err(FixtureError::Unavailable));
        }
        self.peer = Some(output.peer); // Trusted parent's first valid selection, retained across failures/stops.
        if let Some(job) = &mut self.job {
            if tone != job.tone {
                if job.failed.load(Ordering::Acquire)
                    || job.state.load(Ordering::Acquire) != STOPPED
                {
                    return Some(Err(FixtureError::Busy));
                }
            } else {
                let state = job.state.load(Ordering::Acquire);
                if job.failed.load(Ordering::Acquire) {
                    return Some(Err(FixtureError::CleanupFailed));
                }
                if state == OPENING && job.opened.elapsed() >= Duration::from_secs(2) {
                    job.buffer.disable();
                    job.stop.store(true, Ordering::Release);
                    job.failed.store(true, Ordering::Release);
                    return Some(Err(FixtureError::TimedOut));
                }
                return match state {
                    OPENING => None,
                    RUNNING | STOPPED => Some(Ok(())),
                    other => Some(Err(code_error(other))),
                };
            }
        }
        if tone.0 <= self.last {
            return Some(Err(FixtureError::BadCall));
        }
        if OUTPUT_WORKERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 4).then_some(n + 1)
            })
            .is_err()
        {
            return Some(Err(FixtureError::Busy));
        }
        let lease = OutputLease;
        let opened = Instant::now();
        let stopping = Arc::new(std::sync::OnceLock::<Instant>::new());
        let stop_deadline = stopping.clone();
        let buffer = Arc::new(ToneBuffer::new());
        let state = Arc::new(std::sync::atomic::AtomicU8::new(OPENING));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (hal, b, s, halt, uncertain) = (
            self.hal.clone(),
            buffer.clone(),
            state.clone(),
            stop.clone(),
            failed.clone(),
        );
        if thread::Builder::new()
            .name("tutorial-output".into())
            .spawn(move || {
                let mut lease = Some(lease);
                let result = (|| {
                    let expected = hal.inspect()?;
                    expected.admit()?;
                    if halt.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    let mut session = match hal.create(expected.id, b.clone()) {
                        Ok(session) => session,
                        Err(error) => {
                            if error == FixtureError::CleanupFailed {
                                std::mem::forget(lease.take());
                            }
                            return Err(error);
                        }
                    };
                    let operation = (|| {
                        if !expected.same(&hal.inspect()?) {
                            return Err(FixtureError::OutputChanged);
                        }
                        if halt.load(Ordering::Acquire) {
                            return Ok(());
                        }
                        if opened.elapsed() >= Duration::from_secs(2) {
                            uncertain.store(true, Ordering::Release);
                            return Err(FixtureError::TimedOut);
                        }
                        // A stop irreversibly changes armed(1) to disabled(0); never re-enable it.
                        if b.enabled
                            .compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst)
                            .is_err()
                        {
                            return Ok(());
                        }
                        session.start()?;
                        if opened.elapsed() >= Duration::from_secs(2) {
                            uncertain.store(true, Ordering::Release);
                            return Err(FixtureError::TimedOut);
                        }
                        if halt.load(Ordering::Acquire) {
                            b.disable();
                        } else {
                            s.store(RUNNING, Ordering::Release);
                        }
                        while !halt.load(Ordering::Acquire) && !b.finished.load(Ordering::Acquire) {
                            if b.bad.load(Ordering::Acquire) || !expected.same(&hal.inspect()?) {
                                return Err(FixtureError::OutputChanged);
                            }
                            thread::sleep(Duration::from_millis(5));
                        }
                        Ok(())
                    })();
                    stop_deadline.get_or_init(Instant::now);
                    b.disable();
                    let stopped = session.stop();
                    let destroyed = session.destroy();
                    if stopped.is_err() || destroyed.is_err() {
                        std::mem::forget(session);
                        std::mem::forget(lease.take());
                        return Err(FixtureError::CleanupFailed);
                    }
                    if b.bad.load(Ordering::Acquire) {
                        return Err(FixtureError::OutputChanged);
                    }
                    operation
                })();
                b.disable();
                if stop_deadline
                    .get()
                    .is_some_and(|at| at.elapsed() >= Duration::from_millis(50))
                {
                    uncertain.store(true, Ordering::Release);
                }
                let code = if uncertain.load(Ordering::Acquire) {
                    result_code(FixtureError::CleanupFailed)
                } else {
                    match result {
                        Ok(()) => STOPPED,
                        Err(error) => result_code(error),
                    }
                };
                s.store(code, Ordering::Release);
            })
            .is_err()
        {
            return Some(Err(FixtureError::Unavailable));
        }
        self.last = tone.0;
        self.job = Some(ToneJob {
            tone,
            buffer,
            state,
            stop,
            failed,
            opened,
            stopping,
        });
        None
    }
    fn stop(&mut self, tone: ToneId) -> Option<Result<(), FixtureError>> {
        let Some(job) = &mut self.job else {
            return Some(Err(FixtureError::NotOwned));
        };
        if job.tone != tone {
            return Some(Err(FixtureError::NotOwned));
        }
        let at = job.stopping.get_or_init(Instant::now);
        job.buffer.disable();
        job.stop.store(true, Ordering::Release);
        if job.failed.load(Ordering::Acquire) {
            return Some(Err(FixtureError::CleanupFailed));
        }
        let state = job.state.load(Ordering::Acquire);
        if state == STOPPED {
            return Some(Ok(()));
        }
        if state > STOPPED {
            return Some(Err(code_error(state)));
        }
        if at.elapsed() >= Duration::from_millis(50) {
            job.failed.store(true, Ordering::Release);
            return Some(Err(FixtureError::CleanupFailed));
        }
        None
    }
    fn state(&self) -> OwnToneState {
        match &self.job {
            None => OwnToneState::Stopped,
            Some(job)
                if !job.failed.load(Ordering::Acquire)
                    && job.state.load(Ordering::Acquire) == STOPPED =>
            {
                OwnToneState::Stopped
            }
            Some(job)
                if !job.failed.load(Ordering::Acquire)
                    && job.state.load(Ordering::Acquire) == RUNNING =>
            {
                OwnToneState::Running { tone: job.tone }
            }
            Some(job) => OwnToneState::StopUnconfirmed { tone: job.tone },
        }
    }
}
impl Drop for OutputController {
    fn drop(&mut self) {
        if let Some(job) = &self.job {
            job.buffer.disable();
            job.stop.store(true, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod output_tests {
    use super::*;

    #[derive(Clone)]
    struct CallbackHal(Arc<CallbackFacts>);
    struct CallbackFacts {
        inspected: AtomicUsize,
        started: std::sync::atomic::AtomicBool,
        release: std::sync::atomic::AtomicBool,
        buffer: std::sync::Mutex<Option<Arc<ToneBuffer>>>,
    }
    impl TutorialHal for CallbackHal {
        fn inspect(&self) -> Result<OutputDevice, FixtureError> {
            self.0.inspected.fetch_add(1, Ordering::Release);
            wait_fake(|| {
                !self.0.started.load(Ordering::Acquire) || self.0.release.load(Ordering::Acquire)
            });
            Ok(OutputDevice {
                id: 1,
                uid: SPEAKERS_APP_UID.into(),
                alive: true,
                hidden: false,
                class: kAudioDeviceClassID,
                transport: kAudioDeviceTransportTypeVirtual,
                nominal_rate: 48_000.0,
                inputs: vec![],
                outputs: vec![(
                    2,
                    OutputFormat {
                        mSampleRate: 48_000.0,
                        mFormatID: kAudioFormatLinearPCM,
                        mFormatFlags: kAudioFormatFlagsNativeFloatPacked,
                        mBytesPerPacket: 8,
                        mFramesPerPacket: 1,
                        mBytesPerFrame: 8,
                        mChannelsPerFrame: 2,
                        mBitsPerChannel: 32,
                        mReserved: 0,
                    },
                )],
            })
        }
        fn create(
            &self,
            _: u32,
            buffer: Arc<ToneBuffer>,
        ) -> Result<Box<dyn ToneSession>, FixtureError> {
            *self.0.buffer.lock().unwrap() = Some(buffer);
            Ok(Box::new(CallbackSession(self.clone())))
        }
    }
    struct CallbackSession(CallbackHal);
    impl ToneSession for CallbackSession {
        fn start(&mut self) -> Result<(), FixtureError> {
            self.0.0.started.store(true, Ordering::Release);
            Ok(())
        }
        fn stop(&mut self) -> Result<(), FixtureError> {
            Ok(())
        }
        fn destroy(&mut self) -> Result<(), FixtureError> {
            Ok(())
        }
    }
    fn wait_fake(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !predicate() {
            assert!(Instant::now() < deadline, "fake worker did not finish");
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn malformed_then_valid_cannot_succeed(misaligned: bool) {
        let hal = CallbackHal(Arc::new(CallbackFacts {
            inspected: AtomicUsize::new(0),
            started: false.into(),
            release: false.into(),
            buffer: std::sync::Mutex::new(None),
        }));
        let mut controller = OutputController {
            hal: Arc::new(hal.clone()),
            peer: None,
            last: 0,
            job: None,
        };
        let peer = NodeId([8; 32]);
        let selection = SpeakersSelection {
            peer,
            device_key: format!("crosspane.{peer}.speaker"),
        };
        assert_eq!(controller.play(ToneId(1), &selection), None);
        wait_fake(|| hal.0.inspected.load(Ordering::Acquire) >= 3);
        let buffer = hal.0.buffer.lock().unwrap().clone().unwrap();
        let registration = Box::new(Registration {
            entries: AtomicUsize::new(0),
            disabled: false.into(),
            buffer: UnsafeCell::new(Some(buffer.clone())),
        });
        let mut header_bytes = [0x5a5a_5a5a_5a5a_5a5au64; 8];
        let malformed = if misaligned {
            header_bytes
                .as_mut_ptr()
                .cast::<u8>()
                .wrapping_add(1)
                .cast::<AudioBufferList>()
        } else {
            std::ptr::null_mut()
        };
        assert!(!output_header(malformed.addr()));
        // SAFETY: the installed header guard performs no field read for null or this misaligned
        // test-owned allocation; the registration is valid, live and owned by this test.
        unsafe {
            assert_eq!(
                output_callback(
                    1,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    malformed,
                    std::ptr::null(),
                    std::ptr::from_ref(&*registration).cast_mut().cast()
                ),
                0
            );
        }
        let mut samples = [99.0f32; 8192];
        let mut valid = OwnedList {
            count: 1,
            buffers: [AudioBuffer {
                mNumberChannels: 2,
                mDataByteSize: 32768,
                mData: samples.as_mut_ptr().cast(),
            }],
        };
        let mut always_silent = true;
        for _ in 0..24 {
            samples.fill(99.0);
            invoke(&registration, &mut valid);
            always_silent &= samples.iter().all(|sample| *sample == 0.0);
        }
        let latched = bad(&registration);
        hal.0.release.store(true, Ordering::Release);
        wait_fake(|| {
            controller
                .job
                .as_ref()
                .unwrap()
                .state
                .load(Ordering::Acquire)
                > RUNNING
        });
        let result = controller.play(ToneId(1), &selection);
        assert!(latched);
        assert!(always_silent);
        assert_eq!(header_bytes, [0x5a5a_5a5a_5a5a_5a5au64; 8]);
        assert_eq!(result, Some(Err(FixtureError::OutputChanged)));
        assert_eq!(
            controller.state(),
            OwnToneState::StopUnconfirmed { tone: ToneId(1) }
        );
        assert_eq!(
            controller.play(ToneId(2), &selection),
            Some(Err(FixtureError::Busy))
        );
    }
    #[test]
    fn verify2_null_header_then_valid_callbacks_never_reports_stopped() {
        malformed_then_valid_cannot_succeed(false);
    }
    #[test]
    fn verify2_misaligned_header_then_valid_callbacks_never_reports_stopped() {
        malformed_then_valid_cannot_succeed(true);
    }

    // Red evidence first used staged non-null-only predicates, without calling the unchanged
    // callback on invalid pointers. The pure predicate failures were seam-stage evidence.
    #[test]
    fn round1_misaligned_header_integer_admission_is_refused() {
        let owned = [0u64; 8];
        let aligned = owned.as_ptr().addr();
        assert!(output_header(aligned));
        assert!(!output_header(aligned + 1));
    }

    #[test]
    fn round1_wrapping_declared_extent_integer_admission_is_refused() {
        let owned = [0u64; 8];
        assert!(output_range(owned.as_ptr().addr(), size_of_val(&owned)));
        assert!(!output_range(0, 8));
        assert!(!output_range(usize::MAX - 7, 8));
        assert!(!output_range(usize::MAX - 7, 32_768));
    }

    #[test]
    fn round1_misaligned_owned_header_is_rejected_without_field_reads() {
        let registration = registration();
        let mut owned = [0x5a5a_5a5a_5a5a_5a5au64; 8];
        let output = owned
            .as_mut_ptr()
            .cast::<u8>()
            .wrapping_add(1)
            .cast::<AudioBufferList>();
        assert!(!output_header(output.addr()));
        // SAFETY: the installed alignment guard rejects before any header field read. The
        // pointer stays inside a test-owned allocation; registration is valid and test-owned.
        let status = unsafe {
            output_callback(
                9,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                output,
                std::ptr::null(),
                std::ptr::from_ref(&*registration).cast_mut().cast(),
            )
        };
        assert_eq!(status, 0);
        assert_eq!(owned, [0x5a5a_5a5a_5a5a_5a5au64; 8]);
        assert_eq!(registration.entries.load(Ordering::SeqCst), 0);
        assert!(bad(&registration));
    }

    // Lead-approved exception: raw callbacks receive only these owned lists, buffers and
    // registrations. No test calls CoreAudio, opens a device or performs native observation.
    #[repr(C)]
    struct OwnedList<const N: usize> {
        count: u32,
        buffers: [AudioBuffer; N],
    }

    fn registration() -> Box<Registration> {
        let buffer = Arc::new(ToneBuffer::new());
        buffer.enabled.store(2, Ordering::SeqCst);
        Box::new(Registration {
            entries: AtomicUsize::new(0),
            disabled: false.into(),
            buffer: UnsafeCell::new(Some(buffer)),
        })
    }

    fn invoke<const N: usize>(registration: &Registration, list: &mut OwnedList<N>) {
        assert!(list.count as usize <= N);
        // SAFETY: repr(C) gives the same header and descriptor-array offset; every declared
        // descriptor and writable data extent belongs to this test and outlives this call.
        let result = unsafe {
            output_callback(
                9,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::from_mut(list).cast(),
                std::ptr::null(),
                std::ptr::from_ref(registration).cast_mut().cast(),
            )
        };
        assert_eq!(result, 0);
        assert_eq!(registration.entries.load(Ordering::SeqCst), 0);
    }

    fn bad(registration: &Registration) -> bool {
        // SAFETY: this test owns the registration and invokes callbacks synchronously.
        unsafe {
            (&*registration.buffer.get())
                .as_ref()
                .unwrap()
                .bad
                .load(Ordering::Acquire)
        }
    }

    #[test]
    fn callback_writes_only_valid_owned_stereo_output_and_ignores_input() {
        let registration = registration();
        let mut samples = vec![7.0f32; 1024];
        let mut input_samples = vec![19.0f32; 16];
        let input = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer {
                mNumberChannels: 2,
                mDataByteSize: 64,
                mData: input_samples.as_mut_ptr().cast(),
            }],
        };
        let timestamp = [0xa5u8; 64];
        let mut list = OwnedList {
            count: 1,
            buffers: [AudioBuffer {
                mNumberChannels: 2,
                mDataByteSize: (samples.len() * 4) as u32,
                mData: samples.as_mut_ptr().cast(),
            }],
        };
        // SAFETY: all pointers reference live test-owned aligned descriptors/data. Input and
        // timestamp arguments are owned sentinels; the output callback must never read them.
        assert_eq!(
            // SAFETY: the live descriptors and all data extents are owned by this test.
            unsafe {
                output_callback(
                    9,
                    timestamp.as_ptr().cast(),
                    &input,
                    timestamp.as_ptr().cast(),
                    std::ptr::from_mut(&mut list).cast(),
                    timestamp.as_ptr().cast(),
                    std::ptr::from_ref(&*registration).cast_mut().cast(),
                )
            },
            0
        );
        assert!(samples.iter().any(|sample| *sample != 0.0));
        assert!(
            samples
                .as_chunks::<2>()
                .0
                .iter()
                .all(|frame| frame[0] == frame[1])
        );
        assert!(samples.iter().all(|sample| sample.abs() <= 0.0316228));
        assert_eq!(input_samples, vec![19.0; 16]);
        assert_eq!(timestamp, [0xa5; 64]);
        assert_eq!(registration.entries.load(Ordering::SeqCst), 0);
        assert!(!bad(&registration));
    }

    #[test]
    fn callback_rejects_count_and_bounds_descriptor_iteration() {
        for count in [0, 2, 17] {
            let registration = registration();
            let mut data = std::array::from_fn::<_, 17, _>(|_| vec![0x5au8; 32]);
            let mut list: OwnedList<17> = OwnedList {
                count,
                buffers: std::array::from_fn(|i| AudioBuffer {
                    mNumberChannels: 2,
                    mDataByteSize: 32,
                    mData: data[i].as_mut_ptr().cast(),
                }),
            };
            invoke(&registration, &mut list);
            assert!(bad(&registration));
            for (index, bytes) in data.iter().enumerate() {
                let expected = if index < (count as usize).min(MAX_OUTPUT_BUFFERS) {
                    0
                } else {
                    0x5a
                };
                assert!(bytes.iter().all(|byte| *byte == expected));
            }
        }
    }

    #[test]
    fn callback_rejects_channels_size_alignment_and_null_data_with_bounded_zeroing() {
        // Every extent includes a trailing canary outside the descriptor's declared bytes.
        for (channels, size, offset, null) in [
            (1, 8, 0, false),
            (2, 0, 0, false),
            (2, 7, 0, false),
            (2, 8, 1, false),
            (2, MAX_OUTPUT_BYTES + 8, 0, false),
            (2, 8, 0, true),
        ] {
            let registration = registration();
            let mut backing = vec![0x5a5a_5a5au32; (size + offset + 8).div_ceil(4)];
            let base = backing.as_mut_ptr().cast::<u8>();
            let mut list = OwnedList {
                count: 1,
                buffers: [AudioBuffer {
                    mNumberChannels: channels,
                    mDataByteSize: size as u32,
                    mData: if null {
                        std::ptr::null_mut()
                    } else {
                        base.wrapping_add(offset).cast()
                    },
                }],
            };
            invoke(&registration, &mut list);
            assert!(bad(&registration));
            // SAFETY: backing owns this entire initialized byte extent; the callback has returned.
            let bytes =
                unsafe { std::slice::from_raw_parts(base, backing.len() * size_of::<u32>()) };
            let written = if null { 0 } else { size.min(MAX_OUTPUT_BYTES) };
            assert!(bytes[..offset].iter().all(|byte| *byte == 0x5a));
            assert!(
                bytes[offset..offset + written]
                    .iter()
                    .all(|byte| *byte == 0)
            );
            assert!(bytes[offset + written..].iter().all(|byte| *byte == 0x5a));
        }
    }

    #[test]
    fn callback_null_headers_latch_only_for_valid_owned_registration() {
        let registration = registration();
        let mut samples = [7.0f32; 2];
        let mut list = OwnedList {
            count: 1,
            buffers: [AudioBuffer {
                mNumberChannels: 2,
                mDataByteSize: 8,
                mData: samples.as_mut_ptr().cast(),
            }],
        };
        // SAFETY: null arguments deliberately exercise early refusal; the other pointers and
        // descriptor extent are test-owned and the callback cannot dereference a null argument.
        unsafe {
            assert_eq!(
                output_callback(
                    9,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::from_ref(&*registration).cast_mut().cast()
                ),
                0
            );
            assert_eq!(
                output_callback(
                    9,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::from_mut(&mut list).cast(),
                    std::ptr::null(),
                    std::ptr::null_mut()
                ),
                0
            );
        }
        assert_eq!(samples, [7.0; 2]);
        assert_eq!(registration.entries.load(Ordering::SeqCst), 0);
        assert!(bad(&registration));
    }

    #[test]
    fn disabled_late_callback_never_accesses_released_generation() {
        let registration = registration();
        // SAFETY: the test is the sole owner and all callback invocations are synchronous.
        let weak = unsafe { Arc::downgrade((&*registration.buffer.get()).as_ref().unwrap()) };
        registration.disabled.store(true, Ordering::SeqCst);
        assert_eq!(registration.entries.load(Ordering::SeqCst), 0);
        // SAFETY: modeled successful destruction plus disabled/idle quiescence; no concurrent
        // callback exists. The owned registration itself remains alive for the late callback.
        unsafe {
            (*registration.buffer.get()).take();
        }
        assert!(weak.upgrade().is_none());
        let mut samples = [7.0f32; 16];
        let mut list = OwnedList {
            count: 1,
            buffers: [AudioBuffer {
                mNumberChannels: 2,
                mDataByteSize: 64,
                mData: samples.as_mut_ptr().cast(),
            }],
        };
        invoke(&registration, &mut list);
        assert_eq!(samples, [0.0; 16]);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn generation_disable_is_irreversible_and_invalid_safe_render_is_silent() {
        let buffer = ToneBuffer::new();
        buffer.disable();
        assert!(
            buffer
                .enabled
                .compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        );
        for (channels, count) in [(1, 16), (2, 0), (2, 15), (2, MAX_OUTPUT_BYTES / 4 + 2)] {
            let buffer = ToneBuffer::new();
            buffer.enabled.store(2, Ordering::SeqCst);
            let mut samples = vec![7.0; count];
            buffer.render(channels, &mut samples);
            assert!(samples.iter().all(|sample| *sample == 0.0));
            assert!(buffer.bad.load(Ordering::Acquire));
            assert_eq!(buffer.enabled.load(Ordering::SeqCst), 0);
        }
    }
}
