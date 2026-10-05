//! Windows destination-only WASAPI shared playback; no virtual endpoints or microphone opens.
//! \[E\] IAudioClient::Initialize documents EVENTCALLBACK|NOPERSIST in shared mode.
//! <https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclient-initialize>
//! Render owns all COM objects on one MTA thread. The control thread alone delivers events.
//! \[U\] A stalled OS call can outlive a bounded return: disabled workers retain their resources.
//! Gate/stop latch consumption silent immediately; already submitted endpoint audio is not revoked.
//! Device/default changes terminate a stream; only an explicit new open can restart playback.

#![allow(unsafe_code)]

use crate::model::audio::{Converter, MAX_RENDER_FRAMES, MixFormat, StreamControl, wait_open};
use crosspane_platform::{
    AudioCapture, AudioEvent, AudioFormat, AudioHost, AudioKind, AudioPlayback, AudioStop,
    EventSink, IoGate, PlatformError, VirtualPorts,
};
use crosspane_types::id::NodeId;
use rtrb::{Consumer, RingBuffer};
use std::{
    ffi::c_void,
    fmt, mem,
    ptr::{self, null_mut},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{HANDLE, PROPERTYKEY},
        Media::Audio::*,
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
            CoTaskMemFree, CoUninitialize,
        },
    },
    core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface, PCWSTR},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, WAIT_FAILED, WAIT_OBJECT_0},
    System::Threading::{CreateEventW, SetEvent, WaitForSingleObject},
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
    sink: Option<Arc<dyn EventSink<AudioEvent>>>,
}
struct HostState {
    registry: Mutex<Registry>,
    shutdown: AtomicBool,
    observer_done: AtomicBool,
}

/// Destination playback remains available when add_peer is Unsupported (WP-W5.2a item0).
/// Construction touches no endpoint. Native opens require an open gate and explicit invocation.
pub struct WindowsAudioHost {
    gate: Arc<IoGate>,
    state: Arc<HostState>,
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
            })
            .unwrap_or(false)
    }
    pub fn new(gate: Arc<IoGate>) -> Result<Self, PlatformError> {
        let state = Arc::new(HostState {
            registry: Mutex::new(Registry {
                records: Vec::with_capacity(MAX_STREAMS),
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
        Ok(Self { gate, state })
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
            (sink, errors)
        };
        if let Some(sink) = dispatch.0 {
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
    fn add_peer(&mut self, _: NodeId, _: &str) -> Result<VirtualPorts, PlatformError> {
        Err(PlatformError::Unsupported("Windows virtual audio devices"))
    }
    fn remove_peer(&mut self, _: NodeId) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported("Windows virtual audio devices"))
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
