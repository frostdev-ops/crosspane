//! Windows shared playback and projected-process-tree source capture; no microphone opens.
//! \[E\] IAudioClient::Initialize documents EVENTCALLBACK|NOPERSIST in shared mode.
//! <https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclient-initialize>
//! Render owns all COM objects on one MTA thread. The control thread alone delivers events.
//! \[U\] A stalled OS call can outlive a bounded return: disabled workers retain their resources.
//! Gate/stop latch consumption silent immediately; already submitted endpoint audio is not revoked.
//! Device/default changes terminate a stream; only an explicit new open can restart playback.
//! \[E\] Process-tree loopback needs build20348; it does not mute the original application.
//! <https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ns-audioclientactivationparams-audioclient_process_loopback_params>
//! \[U\] The frozen source API carries PIDs, not original process claims. Refreshing projected
//! PIDs and retaining a liveness handle mitigates, but cannot authenticate against PID reuse.

#![allow(unsafe_code)]

use crate::model::audio::{
    BorrowedDescriptor, Converter, MAX_RENDER_FRAMES, MixFormat, SPEAKER_FRAME_SAMPLES,
    SourceLifecycle, SourceSet, SpeakerMixer, StreamControl, wait_open,
};
use crosspane_platform::{
    AudioCapture, AudioDeviceError, AudioEvent, AudioFormat, AudioHost, AudioKind, AudioPlayback,
    AudioStop, EventSink, IoGate, PlatformError, VirtualPorts,
};
use crosspane_types::id::NodeId;
use rtrb::{Consumer, Producer, RingBuffer};
use std::{
    collections::BTreeMap,
    ffi::c_void,
    fmt,
    mem::{self, ManuallyDrop},
    ptr::{self, null_mut},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{HANDLE, PROPERTYKEY},
        Media::Audio::*,
        System::Com::{
            BLOB, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
            CoTaskMemFree, CoUninitialize,
            StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0},
        },
        System::Variant::VT_BLOB,
    },
    core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface, PCWSTR},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, WAIT_FAILED, WAIT_OBJECT_0},
    System::{
        Diagnostics::Debug::OutputDebugStringW,
        LibraryLoader::{GetModuleHandleW, GetProcAddress},
        SystemInformation::OSVERSIONINFOW,
        Threading::{
            CreateEventW, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            SetEvent, WaitForMultipleObjects, WaitForSingleObject,
        },
    },
};

const OPEN_TIME: Duration = Duration::from_secs(2);
const STOP_TIME: Duration = Duration::from_millis(49);
const MAX_STREAMS: usize = 8;
const MAX_ID: usize = 1024;

fn unavailable() -> PlatformError {
    PlatformError::Backend("Windows shared playback unavailable".into())
}
fn locked() -> PlatformError {
    PlatformError::Locked
}
fn lock<T>(m: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>, PlatformError> {
    m.lock().map_err(|_| unavailable())
}

/// The event handle is created on the caller and shared only to signal retirement, never closed
/// while the render worker can wait on it.
struct Wake(windows_sys::Win32::Foundation::HANDLE);
// SAFETY: kernel event handles may be signaled/waited across threads; Arc owns the one close.
unsafe impl Send for Wake {}
// SAFETY: only thread-safe event operations are exposed; handle ownership is immutable.
unsafe impl Sync for Wake {}
impl Wake {
    fn new() -> Result<Self, PlatformError> {
        // SAFETY: unnamed, non-inherited auto-reset event; no owner/global object is opened.
        let handle = unsafe { CreateEventW(ptr::null(), 0, 0, ptr::null()) };
        if handle.is_null() {
            Err(unavailable())
        } else {
            Ok(Self(handle))
        }
    }
    fn signal(&self) {
        // SAFETY: this Arc retains its successful event handle.
        unsafe {
            SetEvent(self.0);
        }
    }
}
impl Drop for Wake {
    fn drop(&mut self) {
        // SAFETY: final Arc owns this event; every native client using it has already retired.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct Stop {
    control: Arc<StreamControl>,
    wake: Arc<Wake>,
    stopped: bool,
}
impl fmt::Debug for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WindowsAudioStop(..)")
    }
}
impl AudioStop for Stop {
    fn stop(&mut self) {
        if mem::replace(&mut self.stopped, true) {
            return;
        }
        self.control.cancel();
        self.wake.signal();
        let deadline = Instant::now() + STOP_TIME;
        self.control.wait_retired(deadline);
    }
}
impl Drop for Stop {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Record {
    control: Arc<StreamControl>,
    wake: Arc<Wake>,
    reported: bool,
}
struct Registry {
    records: Vec<Record>,
    sources: Vec<SourceRecord>,
    sink: Option<Arc<dyn EventSink<AudioEvent>>>,
}
struct HostState {
    registry: Mutex<Registry>,
    shutdown: AtomicBool,
    observer_done: AtomicBool,
}

/// Destination playback stays independent of source-peer admission (WP-W5.2a item0).
/// Construction touches no endpoint. Native opens require an open gate and explicit invocation.
pub struct WindowsAudioHost {
    gate: Arc<IoGate>,
    state: Arc<HostState>,
    peers: BTreeMap<NodeId, SourceHandle>,
    process_loopback: bool,
}
impl fmt::Debug for WindowsAudioHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WindowsAudioHost(..)")
    }
}
impl WindowsAudioHost {
    #[cfg(test)]
    pub(crate) fn fixture_retired(&self) -> bool {
        lock(&self.state.registry)
            .map(|registry| {
                registry
                    .records
                    .iter()
                    .all(|record| record.control.is_retired())
                    && registry
                        .sources
                        .iter()
                        .all(|record| record.control.retired.load(Ordering::Acquire))
            })
            .unwrap_or(false)
    }
    pub fn new(gate: Arc<IoGate>) -> Result<Self, PlatformError> {
        let state = Arc::new(HostState {
            registry: Mutex::new(Registry {
                records: Vec::with_capacity(MAX_STREAMS),
                sources: Vec::with_capacity(MAX_STREAMS),
                sink: None,
            }),
            shutdown: AtomicBool::new(false),
            observer_done: AtomicBool::new(false),
        });
        let owner = state.clone();
        thread::Builder::new()
            .name("crosspane-audio-events".into())
            .spawn(move || observe(owner))
            .map_err(|_| unavailable())?;
        Ok(Self {
            gate,
            state,
            peers: BTreeMap::new(),
            process_loopback: loopback_supported(),
        })
    }
}
fn observe(state: Arc<HostState>) {
    struct Exit(Arc<HostState>);
    impl Drop for Exit {
        fn drop(&mut self) {
            self.0.shutdown.store(true, Ordering::Release);
            let registry = self
                .0
                .registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for record in &registry.records {
                record.control.cancel();
                record.wake.signal();
            }
            for record in &registry.sources {
                record.control.cancel();
                record.wake.signal();
            }
            self.0.observer_done.store(true, Ordering::Release);
        }
    }
    let _exit = Exit(state.clone());
    loop {
        let dispatch = {
            let mut registry = match lock(&state.registry) {
                Ok(v) => v,
                Err(_) => break,
            };
            let sink = registry.sink.clone();
            let mut errors = Vec::with_capacity(MAX_STREAMS);
            let mut source_events = Vec::with_capacity(MAX_STREAMS * 3);
            for record in &mut registry.records {
                if state.shutdown.load(Ordering::Acquire) {
                    record.control.cancel();
                    record.wake.signal();
                } else {
                    record.control.permits();
                }
                if let Some(error) = record.control.reason()
                    && !record.reported
                    && sink.is_some()
                {
                    record.reported = true;
                    errors.push(error);
                }
            }
            registry.records.retain(|r| {
                !r.control.is_retired()
                    || !r.reported && r.control.reason().is_some() && sink.is_none()
            });
            for record in &mut registry.sources {
                if state.shutdown.load(Ordering::Acquire) {
                    record.control.cancel();
                    record.wake.signal();
                }
                while let Ok(event) = record.events.pop() {
                    match event {
                        SourceEvent::Active(active) => {
                            // Preserve genuine zero/positive edges in owner order, even when a
                            // later failure already fenced PCM before this control tick ran.
                            if active != record.sent_active {
                                record.sent_active = active;
                                source_events.push(AudioEvent::VirtualActive {
                                    peer: record.peer,
                                    kind: AudioKind::Speaker,
                                    active,
                                });
                            }
                        }
                        SourceEvent::Error(error) => source_events.push(AudioEvent::DeviceError {
                            peer: Some(record.peer),
                            kind: AudioKind::Speaker,
                            error,
                        }),
                    }
                }
                if !record.control.permits() && record.sent_active {
                    record.sent_active = false;
                    source_events.push(AudioEvent::VirtualActive {
                        peer: record.peer,
                        kind: AudioKind::Speaker,
                        active: false,
                    });
                }
                let fallback = match record.control.fallback_error.swap(0, Ordering::AcqRel) {
                    1 => Some(AudioDeviceError::Unavailable),
                    2 => Some(AudioDeviceError::Locked),
                    3 => Some(AudioDeviceError::PermissionDenied),
                    4 => Some(AudioDeviceError::Failed),
                    _ => None,
                };
                if let Some(error) = fallback {
                    source_events.push(AudioEvent::DeviceError {
                        peer: Some(record.peer),
                        kind: AudioKind::Speaker,
                        error,
                    });
                }
            }
            registry
                .sources
                .retain(|r| !r.control.retired.load(Ordering::Acquire) || r.events.slots() != 0);
            (sink, errors, source_events)
        };
        if let Some(sink) = dispatch.0 {
            for event in dispatch.2 {
                sink.send(event);
            }
            for error in dispatch.1 {
                sink.send(AudioEvent::DeviceError {
                    peer: None,
                    kind: AudioKind::Speaker,
                    error,
                });
            }
        }
        if state.shutdown.load(Ordering::Acquire) {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    state.observer_done.store(true, Ordering::Release);
}
impl AudioHost for WindowsAudioHost {
    fn add_peer(&mut self, peer: NodeId, _: &str) -> Result<VirtualPorts, PlatformError> {
        if self.state.shutdown.load(Ordering::Acquire) || self.peers.contains_key(&peer) {
            return Err(unavailable());
        }
        if self.peers.len() >= 4 || lock(&self.state.registry)?.sources.len() >= MAX_STREAMS {
            return Err(PlatformError::Unsupported("Windows source worker limit"));
        }
        let (speaker, speaker_out) = RingBuffer::new(96_000);
        let (mic_in, microphone) = RingBuffer::<f32>::new(960);
        let (events, event_reader) = RingBuffer::new(64);
        let (send, receive) = mpsc::sync_channel(1);
        let control = Arc::new(SourceControl::new(self.gate.clone()));
        let wake = Arc::new(Wake::new()?);
        let worker_control = control.clone();
        let worker_wake = wake.clone();
        let gate = self.gate.clone();
        let supported = self.process_loopback;
        thread::Builder::new()
            .name("crosspane-process-audio".into())
            .spawn(move || {
                source_worker(
                    gate,
                    supported,
                    receive,
                    speaker,
                    microphone,
                    events,
                    worker_control,
                    worker_wake,
                )
            })
            .map_err(|_| unavailable())?;
        lock(&self.state.registry)?.sources.push(SourceRecord {
            peer,
            control: control.clone(),
            wake: wake.clone(),
            events: event_reader,
            sent_active: false,
        });
        self.peers.insert(
            peer,
            SourceHandle {
                send,
                control,
                wake,
            },
        );
        Ok(VirtualPorts {
            speaker_out,
            mic_in,
        })
    }
    fn remove_peer(&mut self, peer: NodeId) -> Result<(), PlatformError> {
        if let Some(handle) = self.peers.remove(&peer) {
            handle.stop();
        }
        Ok(())
    }
    fn set_peer_sources(&mut self, peer: NodeId, pids: &[u32]) -> Result<(), PlatformError> {
        let handle = self.peers.get(&peer).ok_or(PlatformError::NotFound)?;
        let deadline = Instant::now() + OPEN_TIME;
        if handle.control.eligibility.is_cancelled() {
            return Err(unavailable());
        }
        let (send, receive) = mpsc::sync_channel(1);
        // Invalid/unbounded configuration still reaches the owner to retire obsolete captures,
        // but never copies an unbounded caller slice or silently truncates an authorized set.
        let pids = if pids.len() > crate::model::audio::MAX_PEER_SOURCES {
            vec![0]
        } else {
            pids.to_vec()
        };
        handle
            .send
            .try_send(SourceRequest {
                pids,
                deadline,
                reply: send,
            })
            .map_err(|_| unavailable())?;
        handle.wake.signal();
        loop {
            match receive.recv_timeout(Duration::from_millis(2)) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(unavailable()),
                Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
                Err(_) => {
                    handle.control.cancel();
                    handle.wake.signal();
                    return Err(PlatformError::Timeout);
                }
            }
        }
    }
    fn subscribe(&mut self, sink: Arc<dyn EventSink<AudioEvent>>) -> Result<(), PlatformError> {
        if self.state.shutdown.load(Ordering::Acquire) {
            return Err(unavailable());
        }
        lock(&self.state.registry)?.sink = Some(sink);
        Ok(())
    }
    fn open_capture(&mut self, _: AudioFormat) -> Result<AudioCapture, PlatformError> {
        if !self.gate.is_open() {
            return Err(locked());
        }
        Err(PlatformError::Unsupported("Windows microphone capture"))
    }
    fn open_playback(&mut self, format: AudioFormat) -> Result<AudioPlayback, PlatformError> {
        let deadline = Instant::now() + OPEN_TIME;
        if self.state.shutdown.load(Ordering::Acquire) {
            return Err(unavailable());
        }
        if !self.gate.is_open() {
            return Err(locked());
        }
        if !format.is_valid() {
            return Err(PlatformError::Unsupported("Windows playback input format"));
        }
        let control = Arc::new(StreamControl::new(self.gate.clone()));
        let wake = Arc::new(Wake::new()?);
        {
            let mut registry = lock(&self.state.registry)?;
            if self.state.shutdown.load(Ordering::Acquire) {
                return Err(unavailable());
            }
            if registry.records.len() >= MAX_STREAMS {
                return Err(PlatformError::Unsupported("Windows playback stream limit"));
            }
            registry.records.push(Record {
                control: control.clone(),
                wake: wake.clone(),
                reported: false,
            });
        }
        // Fifty milliseconds of source PCM, independent of the endpoint's native period/rate.
        let (producer, consumer) = RingBuffer::new(format.frame_samples() * 5);
        let (ready, reply) = mpsc::sync_channel(1);
        let (worker_control, worker_wake) = (control.clone(), wake.clone());
        if thread::Builder::new()
            .name("crosspane-wasapi-render".into())
            .spawn(move || {
                worker(
                    format,
                    consumer,
                    worker_control,
                    worker_wake,
                    deadline,
                    ready,
                )
            })
            .is_err()
        {
            control.fail();
            control.retire();
            return Err(unavailable());
        }
        match wait_open(&reply, deadline, &control) {
            Ok(()) => Ok(AudioPlayback::new(
                producer,
                Box::new(Stop {
                    control,
                    wake,
                    stopped: false,
                }),
            )),
            Err(error) => {
                wake.signal();
                Err(error)
            }
        }
    }
}
impl Drop for WindowsAudioHost {
    fn drop(&mut self) {
        for handle in self.peers.values() {
            handle.control.cancel();
            handle.wake.signal();
        }
        self.state.shutdown.store(true, Ordering::Release);
        if let Ok(registry) = lock(&self.state.registry) {
            for record in &registry.records {
                record.control.cancel();
                record.wake.signal();
            }
        }
        let deadline = Instant::now() + STOP_TIME;
        while !self.state.observer_done.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
    }
}

// Source workers own their clients, process liveness handles, PCM scratches and producer. An OS
// stall can retain that owner after the caller's 2s bound; cancellation prevents any later PCM.
// The registry counts retained owners against MAX_STREAMS, so retries cannot grow them unbounded.
struct SourceControl {
    eligibility: SourceLifecycle,
    retired: AtomicBool,
    fallback_error: AtomicU8,
}
impl SourceControl {
    fn new(gate: Arc<IoGate>) -> Self {
        Self {
            eligibility: SourceLifecycle::new(gate),
            retired: AtomicBool::new(false),
            fallback_error: AtomicU8::new(0),
        }
    }
    fn cancel(&self) {
        self.eligibility.cancel();
    }
    fn permits(&self) -> bool {
        self.eligibility.permits()
    }
}
struct SourceHandle {
    send: SyncSender<SourceRequest>,
    control: Arc<SourceControl>,
    wake: Arc<Wake>,
}
impl SourceHandle {
    fn stop(&self) {
        self.control.cancel();
        self.wake.signal();
        let deadline = Instant::now() + STOP_TIME;
        while !self.control.retired.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
    }
}
struct SourceRequest {
    pids: Vec<u32>,
    deadline: Instant,
    reply: SyncSender<Result<(), PlatformError>>,
}
#[derive(Clone, Copy)]
enum SourceEvent {
    Active(bool),
    Error(AudioDeviceError),
}
struct SourceRecord {
    peer: NodeId,
    control: Arc<SourceControl>,
    wake: Arc<Wake>,
    events: Consumer<SourceEvent>,
    sent_active: bool,
}
fn emit(events: &mut Producer<SourceEvent>, event: SourceEvent, control: &SourceControl) {
    // A saturated control-event channel fails closed instead of blocking the audio owner.
    if events.push(event).is_err() {
        let error = match event {
            SourceEvent::Error(AudioDeviceError::Unavailable) => 1,
            SourceEvent::Error(AudioDeviceError::Locked) => 2,
            SourceEvent::Error(AudioDeviceError::PermissionDenied) => 3,
            _ => 4,
        };
        control.fallback_error.store(error, Ordering::Release);
        control.cancel();
    }
}
fn active(events: &mut Producer<SourceEvent>, value: Option<bool>, control: &SourceControl) {
    if let Some(value) = value {
        emit(events, SourceEvent::Active(value), control);
    }
}
fn log_source_failure(pid: u32) {
    let message: Vec<u16> = format!("Crosspane process audio failed pid={pid}\0")
        .encode_utf16()
        .collect();
    // SAFETY: bounded scalar PID-only debug message, no sample, title, path or process content.
    unsafe {
        OutputDebugStringW(message.as_ptr());
    }
}
fn loopback_supported() -> bool {
    // SAFETY: ntdll is already loaded. RtlGetVersion is a documented public version query,
    // resolved with its exact SDK ABI, and writes only this initialized local descriptor.
    unsafe {
        let name: Vec<u16> = "ntdll.dll\0".encode_utf16().collect();
        let module = GetModuleHandleW(name.as_ptr());
        if module.is_null() {
            return false;
        }
        let Some(function) = GetProcAddress(module, c"RtlGetVersion".as_ptr().cast()) else {
            return false;
        };
        let query: unsafe extern "system" fn(*mut OSVERSIONINFOW) -> i32 = mem::transmute(function);
        let mut version = OSVERSIONINFOW {
            dwOSVersionInfoSize: mem::size_of::<OSVERSIONINFOW>() as u32,
            ..Default::default()
        };
        query(&mut version) >= 0 && version.dwMajorVersion >= 10 && version.dwBuildNumber >= 20348
    }
}

#[repr(C)]
struct SourceActivation {
    table: *const IActivateAudioInterfaceCompletionHandler_Vtbl,
    refs: AtomicU32,
    done: Arc<Wake>,
    parameters: AUDIOCLIENT_ACTIVATION_PARAMS,
    variant: BorrowedDescriptor<PROPVARIANT>,
}
unsafe extern "system" fn source_query(
    this: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if iid.is_null() || out.is_null() {
        return HRESULT(0x80004003u32 as i32);
    }
    // SAFETY: COM supplies iid/output. Only IUnknown, our exact handler and IAgileObject exist.
    unsafe {
        *out = null_mut();
        if *iid == IUnknown::IID
            || *iid == IActivateAudioInterfaceCompletionHandler::IID
            || *iid == GUID::from_u128(0x94ea2b94_e9cc_49e0_c0ff_ee64ca8f5b90)
        {
            *out = this;
            source_add_ref(this);
            HRESULT(0)
        } else {
            HRESULT(0x80004002u32 as i32)
        }
    }
}
unsafe extern "system" fn source_add_ref(this: *mut c_void) -> u32 {
    // SAFETY: live COM reference to our reprC allocation; reference count is atomic.
    unsafe {
        (&*this.cast::<SourceActivation>())
            .refs
            .fetch_add(1, Ordering::Relaxed)
            + 1
    }
}
unsafe extern "system" fn source_release(this: *mut c_void) -> u32 {
    // SAFETY: only the final COM reference reclaims this exact owned Box.
    let n = unsafe {
        (&*this.cast::<SourceActivation>())
            .refs
            .fetch_sub(1, Ordering::AcqRel)
            - 1
    };
    if n == 0 {
        // SAFETY: no COM caller or callback retains the allocation after its final release.
        // The borrowed descriptor does not clear inline parameters; this Box alone owns them.
        unsafe {
            drop(Box::from_raw(this.cast::<SourceActivation>()));
        }
    }
    n
}
unsafe extern "system" fn source_complete(this: *mut c_void, _: *mut c_void) -> HRESULT {
    // SAFETY: OS retains the agile handler. Callback only signals its retained event, no locks,
    // allocations, logging, COM result query or sample work.
    unsafe {
        (&*this.cast::<SourceActivation>()).done.signal();
    }
    HRESULT(0)
}
static SOURCE_ACTIVATION: IActivateAudioInterfaceCompletionHandler_Vtbl =
    IActivateAudioInterfaceCompletionHandler_Vtbl {
        base__: IUnknown_Vtbl {
            QueryInterface: source_query,
            AddRef: source_add_ref,
            Release: source_release,
        },
        ActivateCompleted: source_complete,
    };
fn activate_source(
    pid: u32,
    deadline: Instant,
    control: &SourceControl,
) -> Result<IAudioClient, PlatformError> {
    let done = Arc::new(Wake::new()?);
    let mut object = Box::new(SourceActivation {
        table: &SOURCE_ACTIVATION,
        refs: AtomicU32::new(1),
        done: done.clone(),
        parameters: AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        },
        variant: BorrowedDescriptor::new(PROPVARIANT::default()),
    });
    object.variant = BorrowedDescriptor::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: (&mut object.parameters as *mut AUDIOCLIENT_ACTIVATION_PARAMS)
                            .cast(),
                    },
                },
                ..Default::default()
            }),
        },
    });
    let pointer = Box::into_raw(object);
    // SAFETY: exact reprC COM layout; handler owns the initial reference. Inline parameters
    // stay in this stable allocation until its FINAL COM release.
    let handler = unsafe { IActivateAudioInterfaceCompletionHandler::from_raw(pointer.cast()) };
    // SAFETY: public process-tree loopback only; descriptor borrows inline backing retained
    // through the final handler release, including late completion after caller retirement.
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some((*pointer).variant.get()),
            &handler,
        )
    }
    .map_err(|_| unavailable())?;
    let mut expired = false;
    loop {
        // SAFETY: owner retains handler, operation, parameters and event through late completion.
        let result = unsafe { WaitForSingleObject(done.0, 2) };
        expired |= Instant::now() >= deadline || control.eligibility.is_cancelled();
        if result == WAIT_OBJECT_0 {
            break;
        }
        if result == WAIT_FAILED {
            // Retain this bounded owner, not detached uncounted COM references. Caller retirement
            // disables PCM immediately and counts this owner until completion can be observed.
            control.cancel();
            expired = true;
            thread::sleep(Duration::from_millis(2));
        }
    }
    if expired {
        return Err(PlatformError::Timeout);
    }
    let mut result = HRESULT(0x8000000au32 as i32);
    let mut unknown = None;
    // SAFETY: completed owned operation on its MTA; exact initialized outputs.
    unsafe { operation.GetActivateResult(&mut result, &mut unknown) }.map_err(|_| unavailable())?;
    result.ok().map_err(|_| unavailable())?;
    unknown
        .ok_or_else(unavailable)?
        .cast()
        .map_err(|_| unavailable())
}

struct ProcessHandle(windows_sys::Win32::Foundation::HANDLE);
impl Drop for ProcessHandle {
    fn drop(&mut self) {
        // SAFETY: this owner closes only its own successfully opened read-only process handle.
        unsafe {
            CloseHandle(self.0);
        }
    }
}
impl ProcessHandle {
    fn open(pid: u32) -> Result<Self, PlatformError> {
        // SAFETY: read-only liveness, no signals, executable/path query or identity claim.
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        if handle.is_null() {
            Err(unavailable())
        } else {
            Ok(Self(handle))
        }
    }
    fn alive(&self) -> bool {
        // SAFETY: retained process handle; zero wait observes termination without changing it.
        unsafe { WaitForSingleObject(self.0, 0) == windows_sys::Win32::Foundation::WAIT_TIMEOUT }
    }
}
struct SourceCapture {
    pid: u32,
    process: ProcessHandle,
    packets: IAudioCaptureClient,
    audio: IAudioClient,
    event: Arc<Wake>,
    pcm: Producer<f32>,
    mix: Consumer<f32>,
    started: bool,
    ready: bool,
}
impl SourceCapture {
    fn open(
        pid: u32,
        deadline: Instant,
        control: &SourceControl,
        epoch: &StreamControl,
    ) -> Result<Self, PlatformError> {
        let check = || {
            if Instant::now() >= deadline {
                return Err(PlatformError::Timeout);
            }
            if !control.permits() || !epoch.permits() {
                return Err(locked());
            }
            Ok(())
        };
        check()?;
        let process = ProcessHandle::open(pid)?;
        if !process.alive() {
            return Err(unavailable());
        }
        let event = Arc::new(Wake::new()?);
        let audio = activate_source(pid, deadline, control)?;
        check()?;
        let format = WAVEFORMATEX {
            wFormatTag: 3,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 384_000,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        // SAFETY: initialized public shared process-loopback client, checked fixed Speaker format.
        // NOPERSIST is documented for rendering, not asserted for this capture initializer.
        unsafe {
            audio.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK
                    | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                0,
                0,
                &format,
                None,
            )
        }
        .map_err(|_| unavailable())?;
        check()?;
        // SAFETY: exact live event is retained until client Stop and COM release.
        unsafe { audio.SetEventHandle(HANDLE(event.0)) }.map_err(|_| unavailable())?;
        // SAFETY: initialized capture's precise service, owned on the same MTA.
        let packets =
            unsafe { audio.GetService::<IAudioCaptureClient>() }.map_err(|_| unavailable())?;
        let (pcm, mix) = RingBuffer::new(9_600);
        let mut client = Self {
            pid,
            process,
            packets,
            audio,
            event,
            pcm,
            mix,
            started: false,
            ready: false,
        };
        check()?;
        if !client.process.alive() {
            return Err(unavailable());
        }
        // SAFETY: same-thread successfully initialized shared capture, exactly one start.
        unsafe { client.audio.Start() }.map_err(|_| unavailable())?;
        client.started = true;
        check()?;
        Ok(client)
    }
    fn drain(&mut self, control: &SourceControl, epoch: &StreamControl) -> bool {
        if !self.process.alive() {
            return false;
        }
        if !mem::replace(&mut self.ready, false) {
            // SAFETY: own registered event. PCM is drained only on actual WASAPI notification;
            // zero wait collects other signaled clients after the wait-any woke one of them.
            match unsafe { WaitForSingleObject(self.event.0, 0) } {
                WAIT_OBJECT_0 => {}
                windows_sys::Win32::Foundation::WAIT_TIMEOUT => return true,
                _ => return false,
            }
        }
        for _ in 0..32 {
            if !control.permits() || !epoch.permits() {
                return true;
            }
            let mut pending = 0;
            // SAFETY: same-thread initialized capture; raw HRESULT avoids allocations in PCM loop.
            if unsafe {
                (self.packets.vtable().GetNextPacketSize)(self.packets.as_raw(), &mut pending)
            }
            .is_err()
            {
                return false;
            }
            if pending == 0 {
                return true;
            }
            let mut data = null_mut();
            let mut frames = 0;
            let mut flags = 0;
            // SAFETY: exact initialized outputs; one matching release below for each acquisition.
            if unsafe {
                (self.packets.vtable().GetBuffer)(
                    self.packets.as_raw(),
                    &mut data,
                    &mut frames,
                    &mut flags,
                    null_mut(),
                    null_mut(),
                )
            }
            .is_err()
            {
                return false;
            }
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            let valid = frames as usize <= MAX_RENDER_FRAMES && (silent || !data.is_null());
            if valid && epoch.permits() && control.permits() {
                for frame in 0..frames as usize {
                    if self.pcm.slots() < 2 {
                        break;
                    }
                    for channel in 0..2 {
                        let value = if silent {
                            0.0
                        } else {
                            // SAFETY: fixed stereo f32 negotiated; valid packet has frames*8 bytes.
                            unsafe {
                                ptr::read_unaligned(
                                    data.add((frame * 2 + channel) * 4).cast::<f32>(),
                                )
                            }
                        };
                        let _ = self.pcm.push(if value.is_finite() { value } else { 0.0 });
                    }
                }
            }
            // SAFETY: exactly one release, including invalid packets and gate transitions.
            let released =
                unsafe { (self.packets.vtable().ReleaseBuffer)(self.packets.as_raw(), frames) };
            if !valid || released.is_err() {
                return false;
            }
        }
        false
    }
}
impl Drop for SourceCapture {
    fn drop(&mut self) {
        if self.started {
            // SAFETY: own exact initialized client; one Stop, no retry on unknown result.
            let _ = unsafe { (self.audio.vtable().Stop)(self.audio.as_raw()) };
        }
        let _ = &self.event;
    }
}

#[allow(clippy::too_many_arguments)]
fn source_worker(
    gate: Arc<IoGate>,
    supported: bool,
    requests: Receiver<SourceRequest>,
    mut speaker: Producer<f32>,
    _microphone: Consumer<f32>,
    mut events: Producer<SourceEvent>,
    control: Arc<SourceControl>,
    wake: Arc<Wake>,
) {
    struct Retire(Arc<SourceControl>);
    impl Drop for Retire {
        fn drop(&mut self) {
            if thread::panicking() {
                self.0.fallback_error.store(4, Ordering::Release);
            }
            self.0.cancel();
            self.0.retired.store(true, Ordering::Release);
        }
    }
    let _retire = Retire(control.clone());
    let Ok(_apartment) = Apartment::new() else {
        emit(
            &mut events,
            SourceEvent::Error(AudioDeviceError::Failed),
            &control,
        );
        return;
    };
    let mut set = SourceSet::new(supported);
    let mut captures: Vec<SourceCapture> =
        Vec::with_capacity(crate::model::audio::MAX_PEER_SOURCES);
    let mut epoch = StreamControl::new(gate.clone());
    let mut revision = 0;
    let mut mixer = SpeakerMixer::new();
    let mut frame = [0.0; SPEAKER_FRAME_SAMPLES];
    let mut next_mix = Instant::now();
    while !control.eligibility.is_cancelled() {
        if !epoch.permits() {
            control.eligibility.disable();
            active(&mut events, set.gate_closed(), &control);
            captures.clear();
        }
        let request = match requests.try_recv() {
            Ok(request) => Some(request),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => break,
        };
        if let Some(request) = request {
            let result = (|| {
                if Instant::now() >= request.deadline {
                    return Err(PlatformError::Timeout);
                }
                let delta = match set.replace(&request.pids, gate.is_open()) {
                    Ok(delta) => delta,
                    Err(error) => {
                        control.eligibility.disable();
                        active(&mut events, set.gate_closed(), &control);
                        captures.clear();
                        return Err(error);
                    }
                };
                if !delta.changed {
                    return Ok(());
                }
                revision = delta.revision;
                let observed_epoch = gate.epoch();
                epoch = StreamControl::new(gate.clone());
                if delta.active == Some(false) {
                    control.eligibility.disable();
                }
                active(&mut events, delta.active, &control);
                captures.retain(|client| !delta.stop.contains(&client.pid));
                control
                    .eligibility
                    .replace_revision(observed_epoch, !request.pids.is_empty());
                if let Some(error) = delta.error {
                    emit(&mut events, SourceEvent::Error(error), &control);
                }
                for pid in delta.start {
                    match SourceCapture::open(pid, request.deadline, &control, &epoch) {
                        Ok(client) => {
                            captures.push(client);
                            active(&mut events, set.started(revision, pid), &control);
                        }
                        Err(error) => {
                            if !control.permits() || !epoch.permits() {
                                control.eligibility.disable();
                                active(&mut events, set.gate_closed(), &control);
                                captures.clear();
                                return Err(locked());
                            }
                            control.eligibility.disable();
                            if let Some(failure) = set.failed(revision, pid) {
                                active(&mut events, failure.active, &control);
                                emit(&mut events, SourceEvent::Error(failure.error), &control);
                            }
                            captures.clear();
                            log_source_failure(pid);
                            return Err(error);
                        }
                    }
                }
                if !request.pids.is_empty() && (!control.permits() || !epoch.permits()) {
                    control.eligibility.disable();
                    active(&mut events, set.gate_closed(), &control);
                    captures.clear();
                    return Err(locked());
                }
                Ok(())
            })();
            let _ = request.reply.try_send(result);
        }
        if control.eligibility.is_cancelled() {
            break;
        }
        let failed = captures
            .iter_mut()
            .find_map(|client| (!client.drain(&control, &epoch)).then_some(client.pid));
        if let Some(pid) = failed {
            control.eligibility.disable();
            if let Some(failure) = set.failed(revision, pid) {
                active(&mut events, failure.active, &control);
                emit(&mut events, SourceEvent::Error(failure.error), &control);
            }
            captures.clear();
            log_source_failure(pid);
        }
        let now = Instant::now();
        if !captures.is_empty() && now >= next_mix {
            mixer.begin();
            for client in &mut captures {
                mixer.add(&mut client.mix);
            }
            mixer.finish(&mut frame);
            if epoch.permits() && control.permits() {
                for pair in frame.as_chunks::<2>().0 {
                    if speaker.slots() < 2 {
                        break;
                    }
                    if !epoch.permits() || !control.permits() {
                        break;
                    }
                    let _ = speaker.push(pair[0]);
                    let _ = speaker.push(pair[1]);
                }
            }
            next_mix = now + Duration::from_millis(10);
        }
        let mut handles = [null_mut(); crate::model::audio::MAX_PEER_SOURCES + 1];
        handles[0] = wake.0;
        for (index, capture) in captures.iter().enumerate() {
            handles[index + 1] = capture.event.0;
        }
        // SAFETY: every event is retained by this same owner, bounded below MAXIMUM_WAIT_OBJECTS;
        // short timeout observes gate epochs and liveness even when WASAPI produces no packets.
        let waited =
            unsafe { WaitForMultipleObjects((captures.len() + 1) as u32, handles.as_ptr(), 0, 2) };
        if waited == WAIT_FAILED {
            control.fallback_error.store(4, Ordering::Release);
            break;
        }
        if let Some(index) = waited.checked_sub(WAIT_OBJECT_0 + 1)
            && let Some(capture) = captures.get_mut(index as usize)
        {
            capture.ready = true;
        }
    }
    control.eligibility.disable();
    active(&mut events, set.gate_closed(), &control);
    captures.clear();
}

struct Apartment;
impl Apartment {
    fn new() -> Result<Self, PlatformError> {
        // SAFETY: dedicated owned worker has no preexisting COM apartment; balanced on its thread.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }.map_err(|_| unavailable())?;
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: balances this thread's successful initialization after all COM owners drop.
        unsafe {
            CoUninitialize();
        }
    }
}
struct TaskMemory(*mut c_void);
impl Drop for TaskMemory {
    fn drop(&mut self) {
        // SAFETY: GetId/GetMixFormat returned this owned COM-task allocation.
        unsafe {
            CoTaskMemFree(Some(self.0));
        }
    }
}
fn endpoint_id(device: &IMMDevice) -> Result<Vec<u16>, PlatformError> {
    // SAFETY: admitted COM device, public read-only ID query; task allocation immediately guarded.
    let id = unsafe { device.GetId() }.map_err(|_| unavailable())?;
    let _memory = TaskMemory(id.0.cast());
    if id.0.is_null() {
        return Err(unavailable());
    }
    let mut result = Vec::with_capacity(MAX_ID);
    for i in 0..MAX_ID {
        // SAFETY: SDK returns a null-terminated UTF16 ID; bounded walk never parses its contents.
        let value = unsafe { *id.0.add(i) };
        if value == 0 {
            return if result.is_empty() {
                Err(unavailable())
            } else {
                Ok(result)
            };
        }
        result.push(value);
    }
    Err(unavailable())
}

#[repr(C)]
struct Notification {
    table: *const IMMNotificationClient_Vtbl,
    refs: AtomicU32,
    active: AtomicU32,
    enabled: AtomicBool,
    id: Vec<u16>,
    control: Arc<StreamControl>,
}
struct Callback<'a>(&'a Notification);
impl Drop for Callback<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Release);
    }
}
impl Notification {
    unsafe fn enter<'a>(this: *mut c_void) -> Callback<'a> {
        // SAFETY: MMDevice calls our registered COM object; registration retains its reference.
        let me = unsafe { &*this.cast::<Self>() };
        me.active.fetch_add(1, Ordering::Acquire);
        Callback(me)
    }
    fn matches(&self, id: PCWSTR) -> bool {
        if id.0.is_null() {
            return false;
        }
        for (i, expected) in self.id.iter().enumerate() {
            // SAFETY: SDK callback argument is a UTF16 endpoint ID, compared bounded to our ID.
            if unsafe { *id.0.add(i) } != *expected {
                return false;
            }
        }
        // SAFETY: SDK null-terminated string and bounded stored ID length.
        unsafe { *id.0.add(self.id.len()) == 0 }
    }
}
unsafe extern "system" fn query(
    this: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if iid.is_null() || out.is_null() {
        return HRESULT(0x80004003u32 as i32);
    }
    // SAFETY: COM QueryInterface caller supplies valid iid/output; only our two interfaces exist.
    unsafe {
        *out = null_mut();
        if *iid == IMMNotificationClient::IID || *iid == IUnknown::IID {
            *out = this;
            add_ref(this);
            HRESULT(0)
        } else {
            HRESULT(0x80004002u32 as i32)
        }
    }
}
unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    // SAFETY: live COM reference, immutable object with atomic count.
    let me = unsafe { &*this.cast::<Notification>() };
    me.refs.fetch_add(1, Ordering::Relaxed) + 1
}
unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    // SAFETY: live COM reference; only final release reconstructs the uniquely allocated Box.
    let me = unsafe { &*this.cast::<Notification>() };
    let refs = me.refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if refs == 0 {
        // SAFETY: registration is retired and no callback is active before the final owned release.
        unsafe {
            drop(Box::from_raw(this.cast::<Notification>()));
        }
    }
    refs
}
unsafe extern "system" fn changed(this: *mut c_void, id: PCWSTR, _: DEVICE_STATE) -> HRESULT {
    // SAFETY: SDK callback, registered lifetime retained; guard counts only atomically.
    let call = unsafe { Notification::enter(this) };
    if call.0.enabled.load(Ordering::Acquire) && call.0.matches(id) {
        call.0.control.fail();
    }
    HRESULT(0)
}
unsafe extern "system" fn removed(this: *mut c_void, id: PCWSTR) -> HRESULT {
    // SAFETY: same registered callback and ID contract as changed; no blocking/native calls.
    unsafe { changed(this, id, DEVICE_STATE(0)) }
}
unsafe extern "system" fn added(_: *mut c_void, _: PCWSTR) -> HRESULT {
    HRESULT(0)
}
unsafe extern "system" fn default_changed(
    this: *mut c_void,
    flow: EDataFlow,
    role: ERole,
    id: PCWSTR,
) -> HRESULT {
    // SAFETY: SDK callback and retained object.
    let call = unsafe { Notification::enter(this) };
    if call.0.enabled.load(Ordering::Acquire)
        && flow == eRender
        && role == eConsole
        && !call.0.matches(id)
    {
        call.0.control.fail();
    }
    HRESULT(0)
}
unsafe extern "system" fn property(this: *mut c_void, id: PCWSTR, _: PROPERTYKEY) -> HRESULT {
    // SAFETY: property changes on the selected endpoint conservatively invalidate its mix.
    unsafe { changed(this, id, DEVICE_STATE(0)) }
}
static NOTIFICATION: IMMNotificationClient_Vtbl = IMMNotificationClient_Vtbl {
    base__: IUnknown_Vtbl {
        QueryInterface: query,
        AddRef: add_ref,
        Release: release,
    },
    OnDeviceStateChanged: changed,
    OnDeviceAdded: added,
    OnDeviceRemoved: removed,
    OnDefaultDeviceChanged: default_changed,
    OnPropertyValueChanged: property,
};
struct Registration {
    enumerator: IMMDeviceEnumerator,
    callback: Option<IMMNotificationClient>,
}
impl Registration {
    fn new(
        enumerator: IMMDeviceEnumerator,
        id: Vec<u16>,
        control: Arc<StreamControl>,
    ) -> Result<Self, PlatformError> {
        let object = Box::new(Notification {
            table: &NOTIFICATION,
            refs: AtomicU32::new(1),
            active: AtomicU32::new(0),
            enabled: AtomicBool::new(true),
            id,
            control,
        });
        // SAFETY: reprC object's first member is its exact COM vtable; interface owns initial ref.
        let callback = unsafe { IMMNotificationClient::from_raw(Box::into_raw(object).cast()) };
        // SAFETY: reference is held through successful unregister; callback performs only atomics.
        unsafe { enumerator.RegisterEndpointNotificationCallback(&callback) }
            .map_err(|_| unavailable())?;
        Ok(Self {
            enumerator,
            callback: Some(callback),
        })
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        let Some(callback) = self.callback.take() else {
            return;
        };
        // SAFETY: interface is exactly our registered Notification and remains held here.
        let object = unsafe { &*callback.as_raw().cast::<Notification>() };
        object.enabled.store(false, Ordering::Release);
        // SAFETY: called from owner, never callback; reference retained until retirement confirmed.
        let stopped = unsafe {
            self.enumerator
                .UnregisterEndpointNotificationCallback(&callback)
        }
        .is_ok();
        let deadline = Instant::now() + Duration::from_millis(5);
        while stopped && object.active.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        if !stopped || object.active.load(Ordering::Acquire) != 0 {
            // Disabled reference is retained if callback quiescence cannot be established.
            mem::forget(callback);
        }
    }
}

struct Client {
    audio: IAudioClient,
    render: IAudioRenderClient,
    mix: MixFormat,
    converter: Converter,
    frames: u32,
    registration: Option<Registration>,
    // Client objects must retire before their registered event is closed.
    wake: Arc<Wake>,
    started: bool,
}
impl Client {
    fn open(
        format: AudioFormat,
        control: &Arc<StreamControl>,
        wake: Arc<Wake>,
        deadline: Instant,
    ) -> Result<Self, PlatformError> {
        let check = || {
            if Instant::now() >= deadline {
                return Err(PlatformError::Timeout);
            }
            if !control.permits() {
                return Err(control.open_refusal());
            }
            Ok(())
        };
        check()?;
        // SAFETY: public MMDevice enumerator in this worker's MTA; no settings are modified.
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER) }
                .map_err(|_| unavailable())?;
        check()?;
        // SAFETY: read-only current console render endpoint; never enumerates/captures others.
        let device =
            unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }.map_err(|e| {
                if e.code().0 == 0x80070490u32 as i32 {
                    PlatformError::NotFound
                } else {
                    unavailable()
                }
            })?;
        check()?;
        let id = endpoint_id(&device)?;
        let registration = Registration::new(enumerator.clone(), id.clone(), control.clone())?;
        check()?;
        // SAFETY: read-back after registering closes the snapshot/subscribe race.
        let current = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .map_err(|_| unavailable())?;
        if endpoint_id(&current)? != id {
            control.fail();
            return Err(unavailable());
        }
        check()?;
        // SAFETY: only selected default shared renderer is activated, under open gate.
        let audio: IAudioClient =
            unsafe { device.Activate(CLSCTX_INPROC_SERVER, None) }.map_err(|_| unavailable())?;
        check()?;
        // SAFETY: SDK owns the descriptor until CoTaskMemFree; immediately guarded.
        let pointer = unsafe { audio.GetMixFormat() }.map_err(|_| unavailable())?;
        let _memory = TaskMemory(pointer.cast());
        if pointer.is_null() {
            return Err(unavailable());
        }
        // SAFETY: valid SDK WAVEFORMATEX header, packed cbSize read without aligned reference.
        let extension = unsafe { ptr::read_unaligned(ptr::addr_of!((*pointer).cbSize)) };
        if extension > 22 {
            return Err(PlatformError::Unsupported("Windows mix extension"));
        }
        // SAFETY: SDK allocates the header+cbSize descriptor, bounded here to40bytes.
        let bytes = unsafe {
            std::slice::from_raw_parts(pointer.cast::<u8>(), 18 + usize::from(extension))
        };
        let mix = MixFormat::parse(bytes)?;
        let converter = Converter::new(format, mix)?;
        check()?;
        // SAFETY: documented shared initializer, exact checked mix, no period/default/settings change.
        unsafe {
            audio.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_NOPERSIST,
                0,
                0,
                pointer,
                None,
            )
        }
        .map_err(|_| unavailable())?;
        check()?;
        // SAFETY: initialized selected client queried for its precise buffer.
        let frames = unsafe { audio.GetBufferSize() }.map_err(|_| unavailable())?;
        if frames == 0 || frames as usize > MAX_RENDER_FRAMES {
            return Err(unavailable());
        }
        // SAFETY: event Arc lives through this client's teardown; only render service requested.
        unsafe { audio.SetEventHandle(HANDLE(wake.0)) }.map_err(|_| unavailable())?;
        check()?;
        // SAFETY: render service belongs to this successfully initialized shared client.
        let render =
            unsafe { audio.GetService::<IAudioRenderClient>() }.map_err(|_| unavailable())?;
        check()?;
        Ok(Self {
            audio,
            render,
            mix,
            converter,
            frames,
            registration: Some(registration),
            wake,
            started: false,
        })
    }
    fn start(&mut self) -> Result<(), PlatformError> {
        // SAFETY: initialized owned shared client, no preexisting start.
        unsafe { self.audio.Start() }.map_err(|_| unavailable())?;
        self.started = true;
        Ok(())
    }
    fn cycle(&mut self, consumer: &mut Consumer<f32>, control: &StreamControl) -> bool {
        if !control.permits() {
            return false;
        }
        let mut padding = 0;
        // SAFETY: same-thread owned COM object. Raw HRESULT path allocates no error object.
        let result =
            unsafe { (self.audio.vtable().GetCurrentPadding)(self.audio.as_raw(), &mut padding) };
        if result.is_err() || padding > self.frames {
            control.fail();
            return false;
        }
        if !control.permits() {
            return false;
        }
        let frames = self.frames - padding;
        if frames == 0 {
            return true;
        }
        let mut data = null_mut();
        // SAFETY: bounded available frame count and initialized output; acquire/release same owner.
        let result =
            unsafe { (self.render.vtable().GetBuffer)(self.render.as_raw(), frames, &mut data) };
        if result.is_err() {
            control.fail();
            return false;
        }
        let extent = match self.mix.bytes_for(frames as usize) {
            Ok(v) => v,
            Err(_) => {
                control.fail();
                0
            }
        };
        let mut silent = !control.permits() || data.is_null() || extent == 0;
        if !silent {
            // SAFETY: successful GetBuffer grants exactly frames*checkedblockalign writable bytes.
            let bytes = unsafe { std::slice::from_raw_parts_mut(data, extent) };
            if self
                .converter
                .render(consumer, frames as usize, bytes)
                .is_err()
            {
                control.fail();
                silent = true;
            }
        }
        silent |= !control.permits();
        // SAFETY: one release for this acquisition; silence flag avoids submitting stale bytes.
        let result = unsafe {
            (self.render.vtable().ReleaseBuffer)(
                self.render.as_raw(),
                frames,
                if silent {
                    AUDCLNT_BUFFERFLAGS_SILENT.0 as u32
                } else {
                    0
                },
            )
        };
        if result.is_err() {
            control.fail();
            return false;
        }
        control.permits()
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        if self.started {
            // SAFETY: owned initialized client, retired by same owner; no blind retry on failure.
            let _ = unsafe { (self.audio.vtable().Stop)(self.audio.as_raw()) };
        }
        // Registration is explicitly retired while clients/event remain owned; then field teardown.
        drop(self.registration.take());
        let _ = &self.wake;
    }
}

fn worker(
    format: AudioFormat,
    mut pcm: Consumer<f32>,
    control: Arc<StreamControl>,
    wake: Arc<Wake>,
    deadline: Instant,
    ready: SyncSender<Result<(), PlatformError>>,
) {
    struct Retire(Arc<StreamControl>);
    impl Drop for Retire {
        fn drop(&mut self) {
            self.0.retire();
        }
    }
    let _retire = Retire(control.clone());
    let result = (|| {
        let _apartment = Apartment::new()?;
        let mut client = Client::open(format, &control, wake.clone(), deadline)?;
        if Instant::now() >= deadline {
            return Err(PlatformError::Timeout);
        }
        if !control.permits() {
            return Err(control.open_refusal());
        }
        client.start()?;
        if Instant::now() >= deadline {
            return Err(PlatformError::Timeout);
        }
        if !control.permits() {
            return Err(control.open_refusal());
        }
        if ready.try_send(Ok(())).is_err() {
            control.cancel();
            return Ok(());
        }
        while control.permits() {
            // SAFETY: live owned event; short wait also observes gate epochs even without audio events.
            let waited = unsafe { WaitForSingleObject(wake.0, 2) };
            if waited == WAIT_FAILED {
                control.fail();
                break;
            }
            if waited == WAIT_OBJECT_0 && !client.cycle(&mut pcm, &control) {
                break;
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        if !matches!(error, PlatformError::Locked | PlatformError::Timeout) {
            control.fail();
        }
        let _ = ready.try_send(Err(error));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn gate() -> Arc<IoGate> {
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        gate
    }
    #[test]
    fn physical_device_failure_is_reported_once_outside_render_thread() {
        let mut host = WindowsAudioHost::new(gate()).unwrap();
        let (send, receive) = mpsc::channel();
        host.subscribe(Arc::new(move |event| {
            send.send(event).unwrap();
        }))
        .unwrap();
        let control = Arc::new(StreamControl::new(host.gate.clone()));
        lock(&host.state.registry).unwrap().records.push(Record {
            control: control.clone(),
            wake: Arc::new(Wake::new().unwrap()),
            reported: false,
        });
        control.fail();
        assert_eq!(
            receive.recv_timeout(Duration::from_secs(1)).unwrap(),
            AudioEvent::DeviceError {
                peer: None,
                kind: AudioKind::Speaker,
                error: crosspane_platform::AudioDeviceError::Failed,
            }
        );
        assert!(receive.recv_timeout(Duration::from_millis(10)).is_err());
        assert!(!control.permits());
        control.retire();
        assert!(host.fixture_retired());
    }
    #[test]
    fn uncooperative_stop_waits_only_once_across_stop_and_drop() {
        let control = Arc::new(StreamControl::new(gate()));
        let mut stop = Stop {
            control: control.clone(),
            wake: Arc::new(Wake::new().unwrap()),
            stopped: false,
        };
        stop.stop();
        assert!(!control.permits());
        assert!(!control.is_retired());
        let start = Instant::now();
        stop.stop();
        drop(stop);
        assert!(
            start.elapsed() < Duration::from_millis(10),
            "idempotent stop unexpectedly waited"
        );
        assert!(!control.is_retired());
    }
    #[test]
    fn host_drop_disables_live_and_pending_streams_without_opening_devices() {
        let host = WindowsAudioHost::new(gate()).unwrap();
        let control = Arc::new(StreamControl::new(host.gate.clone()));
        lock(&host.state.registry).unwrap().records.push(Record {
            control: control.clone(),
            wake: Arc::new(Wake::new().unwrap()),
            reported: false,
        });
        drop(host);
        assert!(!control.permits());
        assert!(!control.is_retired());
        control.retire();
    }
}
