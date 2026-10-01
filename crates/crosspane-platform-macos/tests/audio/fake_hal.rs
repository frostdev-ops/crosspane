//! A fake CoreAudio HAL for the WP-3.5 tests: the four Crosspane loopback devices plus a built-in
//! output, with call recording, change notifications, injected failures and IO sessions whose
//! callbacks the tests drive directly. Nothing here touches the real HAL.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crosspane_platform_macos::audio::hal::{
    CLASS_AUDIO_DEVICE, DeviceId, DeviceInfo, Hal, HalError, IoBuffer, IoCallback, IoSession,
    ListenTarget, ListenerId, Notifier, StreamFormat, StreamId, StreamInfo, TRANSPORT_VIRTUAL,
};
use crosspane_platform_macos::audio::{
    MIC_APP_UID, MIC_LOOPBACK_UID, SPEAKERS_APP_UID, SPEAKERS_LOOPBACK_UID,
};

/// Device ids are deliberately not in contract order.
pub const SPEAKERS_APP: DeviceId = DeviceId(41);
pub const SPEAKERS_LOOP: DeviceId = DeviceId(17);
pub const MIC_APP: DeviceId = DeviceId(29);
pub const MIC_LOOP: DeviceId = DeviceId(5);
/// A built-in output, the default after `FakeHal::new`.
pub const BUILTIN: DeviceId = DeviceId(100);
/// A second physical output.
pub const OTHER_OUTPUT: DeviceId = DeviceId(101);

pub const TRANSPORT_BUILTIN: u32 = 0x626c_746e;

#[derive(Clone, Debug, PartialEq)]
pub enum Call {
    Translate(String),
    Info(DeviceId),
    Alive(DeviceId),
    Running(DeviceId),
    DefaultOutput,
    AddListener(ListenTarget),
    RemoveListener(ListenTarget),
    Flush,
    StartIo(DeviceId),
    StopIo(DeviceId),
}

struct Device {
    info: DeviceInfo,
    /// Visible-device clients (apps that started IO).
    clients: u32,
}

struct Session {
    device: DeviceId,
    /// Released when the session is dropped, as the HAL releases its client data; a leaked
    /// session keeps it.
    callback: Option<Arc<dyn IoCallback>>,
    stopped: bool,
}

struct State {
    devices: HashMap<u32, Device>,
    uids: HashMap<String, u32>,
    default_output: Option<DeviceId>,
    listeners: HashMap<u64, (ListenTarget, Arc<Notifier>)>,
    next_listener: u64,
    sessions: HashMap<u64, Session>,
    next_session: u64,
    calls: Vec<Call>,
    started: usize,
    stopped: usize,
    dropped: usize,
}

/// A manually released block: threads wait in `wait` until `release` is called.
#[derive(Default)]
pub struct Latch {
    open: Mutex<bool>,
    cv: Condvar,
}

impl Latch {
    pub fn new_closed() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.cv.notify_all();
    }

    pub fn wait(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut open = self.open.lock().unwrap();
        while !*open && Instant::now() < deadline {
            open = self
                .cv
                .wait_timeout(open, Duration::from_millis(50))
                .unwrap()
                .0;
        }
    }
}

#[derive(Default)]
struct Hooks {
    translate_error: Option<HalError>,
    start_error: Option<HalError>,
    stop_delay: Duration,
    /// The next fired notification is delivered on its own thread, held by this latch.
    hold_notification: Option<Arc<Latch>>,
    on_add_listener: Option<Box<dyn FnMut(ListenTarget) + Send>>,
    block_start: Option<Arc<Latch>>,
    block_translate: Option<Arc<Latch>>,
    on_running: Option<Box<dyn FnMut(DeviceId) + Send>>,
    info_error: HashMap<u32, HalError>,
}

struct Shared {
    state: Mutex<State>,
    hooks: Mutex<Hooks>,
    /// Notifications delivered asynchronously (held) and not yet finished: what a real listener
    /// queue's flush has to wait for.
    pending_notifications: AtomicUsize,
    /// `stop` calls currently inside the fake (before they return).
    stopping: AtomicUsize,
}

pub struct FakeHal {
    shared: Arc<Shared>,
}

pub fn contract_streams(channels: u32, id: u32) -> Vec<StreamInfo> {
    vec![StreamInfo {
        id: StreamId(id),
        format: StreamFormat::float32(channels, true),
    }]
}

fn device(uid: &str, hidden: bool, input: Vec<StreamInfo>, output: Vec<StreamInfo>) -> DeviceInfo {
    DeviceInfo {
        uid: uid.to_string(),
        class_id: CLASS_AUDIO_DEVICE,
        transport: TRANSPORT_VIRTUAL,
        alive: true,
        hidden,
        nominal_rate: 48_000.0,
        input_streams: input,
        output_streams: output,
    }
}

impl FakeHal {
    /// The four conforming Crosspane devices and a stereo interleaved built-in default output.
    pub fn new() -> Arc<Self> {
        let mut devices = HashMap::new();
        let mut uids = HashMap::new();
        let mut add = |id: DeviceId, info: DeviceInfo| {
            uids.insert(info.uid.clone(), id.0);
            devices.insert(id.0, Device { info, clients: 0 });
        };
        add(
            SPEAKERS_APP,
            device(SPEAKERS_APP_UID, false, vec![], contract_streams(2, 1001)),
        );
        add(
            SPEAKERS_LOOP,
            device(
                SPEAKERS_LOOPBACK_UID,
                true,
                contract_streams(2, 1002),
                vec![],
            ),
        );
        add(
            MIC_APP,
            device(MIC_APP_UID, false, contract_streams(1, 1003), vec![]),
        );
        add(
            MIC_LOOP,
            device(MIC_LOOPBACK_UID, true, vec![], contract_streams(1, 1004)),
        );
        let mut builtin = device(
            "BuiltInSpeakerDevice",
            false,
            vec![],
            contract_streams(2, 2001),
        );
        builtin.transport = TRANSPORT_BUILTIN;
        add(BUILTIN, builtin.clone());
        let mut other = builtin;
        other.uid = "OtherOutputDevice".to_string();
        other.output_streams = contract_streams(2, 2002);
        add(OTHER_OUTPUT, other);
        Arc::new(Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    devices,
                    uids,
                    default_output: Some(BUILTIN),
                    listeners: HashMap::new(),
                    next_listener: 1,
                    sessions: HashMap::new(),
                    next_session: 1,
                    calls: Vec::new(),
                    started: 0,
                    stopped: 0,
                    dropped: 0,
                }),
                hooks: Mutex::new(Hooks::default()),
                pending_notifications: AtomicUsize::new(0),
                stopping: AtomicUsize::new(0),
            }),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.shared.state.lock().unwrap()
    }

    fn hooks(&self) -> MutexGuard<'_, Hooks> {
        self.shared.hooks.lock().unwrap()
    }

    // -- inspection -------------------------------------------------------------------------

    pub fn calls(&self) -> Vec<Call> {
        self.state().calls.clone()
    }

    pub fn clear_calls(&self) {
        self.state().calls.clear();
    }

    pub fn count(&self, predicate: impl Fn(&Call) -> bool) -> usize {
        self.state().calls.iter().filter(|c| predicate(c)).count()
    }

    pub fn listener_count(&self) -> usize {
        self.state().listeners.len()
    }

    pub fn listener_targets(&self) -> Vec<ListenTarget> {
        self.state().listeners.values().map(|(t, _)| *t).collect()
    }

    /// Clones of every registered notifier (to fire late, after the host removed its listeners).
    pub fn notifiers(&self) -> Vec<Arc<Notifier>> {
        self.state()
            .listeners
            .values()
            .map(|(_, n)| n.clone())
            .collect()
    }

    /// IOProcs created (started) so far, ever.
    pub fn started_total(&self) -> usize {
        self.state().started
    }

    pub fn started_on(&self, device: DeviceId) -> usize {
        self.count(|c| *c == Call::StartIo(device))
    }

    /// Sessions whose `stop` ran.
    pub fn stopped_total(&self) -> usize {
        self.state().stopped
    }

    /// Sessions that were dropped (a leaked session is never dropped).
    pub fn dropped_total(&self) -> usize {
        self.state().dropped
    }

    /// Started and not yet stopped.
    pub fn active_sessions(&self) -> usize {
        self.state()
            .sessions
            .values()
            .filter(|s| !s.stopped)
            .count()
    }

    pub fn active_on(&self, device: DeviceId) -> usize {
        self.state()
            .sessions
            .values()
            .filter(|s| !s.stopped && s.device == device)
            .count()
    }

    /// The callback of the (single) active session on `device`.
    pub fn callback(&self, device: DeviceId) -> Option<Arc<dyn IoCallback>> {
        self.state()
            .sessions
            .values()
            .find(|s| !s.stopped && s.device == device)
            .and_then(|s| s.callback.clone())
    }

    /// The callbacks of every active session on `device`, oldest first.
    pub fn callbacks(&self, device: DeviceId) -> Vec<Arc<dyn IoCallback>> {
        let state = self.state();
        let mut sessions: Vec<_> = state
            .sessions
            .iter()
            .filter(|(_, s)| !s.stopped && s.device == device)
            .collect();
        sessions.sort_by_key(|(id, _)| **id);
        sessions
            .into_iter()
            .filter_map(|(_, s)| s.callback.clone())
            .collect()
    }

    /// The callback of the most recently created session on `device`, stopped or not.
    pub fn last_callback(&self, device: DeviceId) -> Option<Arc<dyn IoCallback>> {
        let state = self.state();
        state
            .sessions
            .iter()
            .filter(|(_, s)| s.device == device)
            .max_by_key(|(id, _)| **id)
            .and_then(|(_, s)| s.callback.clone())
    }

    // -- mutation ---------------------------------------------------------------------------

    pub fn edit(&self, device: DeviceId, change: impl FnOnce(&mut DeviceInfo)) {
        change(&mut self.state().devices.get_mut(&device.0).unwrap().info);
    }

    pub fn set_translate(&self, uid: &str, device: Option<DeviceId>) {
        let mut state = self.state();
        match device {
            Some(id) => state.uids.insert(uid.to_string(), id.0),
            None => state.uids.remove(uid),
        };
    }

    pub fn remove_plugin(&self) {
        for uid in [
            SPEAKERS_APP_UID,
            SPEAKERS_LOOPBACK_UID,
            MIC_APP_UID,
            MIC_LOOPBACK_UID,
        ] {
            self.set_translate(uid, None);
        }
    }

    pub fn fail_translate(&self, error: Option<HalError>) {
        self.hooks().translate_error = error;
    }

    pub fn fail_start(&self, error: Option<HalError>) {
        self.hooks().start_error = error;
    }

    pub fn fail_info(&self, device: DeviceId, error: Option<HalError>) {
        let mut hooks = self.hooks();
        match error {
            Some(e) => hooks.info_error.insert(device.0, e),
            None => hooks.info_error.remove(&device.0),
        };
    }

    pub fn slow_stop(&self, delay: Duration) {
        self.hooks().stop_delay = delay;
    }

    /// Deliver the next fired notification on its own thread, blocked until the latch is released:
    /// a queued listener block that is running (or stuck) when the host retires its listeners.
    pub fn hold_next_notification(&self) -> Arc<Latch> {
        let latch = Latch::new_closed();
        self.hooks().hold_notification = Some(latch.clone());
        latch
    }

    /// Held notifications that have not completed yet.
    pub fn pending_notifications(&self) -> usize {
        self.shared.pending_notifications.load(Ordering::SeqCst)
    }

    /// `stop` calls currently running inside the fake.
    pub fn stopping_now(&self) -> usize {
        self.shared.stopping.load(Ordering::SeqCst)
    }

    /// Run `hook` on every `add_listener` call, before it is recorded (no locks held).
    pub fn on_add_listener(&self, hook: impl FnMut(ListenTarget) + Send + 'static) {
        self.hooks().on_add_listener = Some(Box::new(hook));
    }

    /// The device object disappears: every read of it fails with `Gone`.
    pub fn vanish(&self, device: DeviceId) {
        self.state().devices.remove(&device.0);
        self.notify(ListenTarget::DeviceAlive(device));
        self.notify(ListenTarget::DeviceList);
    }

    /// The plug-in reloads: `uid` now translates to a new object that has the same properties.
    pub fn replace_object(&self, uid: &str, new_id: DeviceId) {
        let mut state = self.state();
        let old = state.uids.get(uid).copied().expect("known uid");
        let moved = Device {
            info: state.devices[&old].info.clone(),
            clients: 0,
        };
        state.devices.insert(new_id.0, moved);
        state.uids.insert(uid.to_string(), new_id.0);
        drop(state);
        self.notify(ListenTarget::DeviceList);
        self.notify(ListenTarget::ServiceRestarted);
    }

    /// Block the next `start_io` until the latch is released.
    pub fn hold_start(&self) -> Arc<Latch> {
        let latch = Latch::new_closed();
        self.hooks().block_start = Some(latch.clone());
        latch
    }

    /// Block the next `translate_uid` until the latch is released.
    pub fn hold_translate(&self) -> Arc<Latch> {
        let latch = Latch::new_closed();
        self.hooks().block_translate = Some(latch.clone());
        latch
    }

    /// Run `hook` after every `is_running_somewhere` read (with no locks held).
    pub fn on_running_read(&self, hook: impl FnMut(DeviceId) + Send + 'static) {
        self.hooks().on_running = Some(Box::new(hook));
    }

    fn fire(&self, matches: impl Fn(&ListenTarget) -> bool) {
        let notifiers: Vec<Arc<Notifier>> = self
            .state()
            .listeners
            .values()
            .filter(|(t, _)| matches(t))
            .map(|(_, n)| n.clone())
            .collect();
        let hold = if notifiers.is_empty() {
            None
        } else {
            self.hooks().hold_notification.take()
        };
        for (index, notifier) in notifiers.into_iter().enumerate() {
            match (&hold, index) {
                (Some(latch), 0) => {
                    // Delivered by a queue thread that is stuck until the latch opens.
                    let latch = latch.clone();
                    let shared = self.shared.clone();
                    shared.pending_notifications.fetch_add(1, Ordering::SeqCst);
                    std::thread::spawn(move || {
                        latch.wait();
                        notifier.notify();
                        shared.pending_notifications.fetch_sub(1, Ordering::SeqCst);
                    });
                }
                _ => notifier.notify(),
            }
        }
    }

    pub fn notify(&self, target: ListenTarget) {
        self.fire(|t| *t == target);
    }

    pub fn notify_all(&self) {
        self.fire(|_| true);
    }

    /// An application starts IO on a device: `DeviceIsRunningSomewhere` flips on the first client.
    pub fn app_start(&self, device: DeviceId) {
        let flipped = {
            let mut state = self.state();
            let d = state.devices.get_mut(&device.0).unwrap();
            d.clients += 1;
            d.clients == 1
        };
        if flipped {
            self.notify(ListenTarget::DeviceRunning(device));
        }
    }

    /// An application stops IO: the property flips off with the last client.
    pub fn app_stop(&self, device: DeviceId) {
        let flipped = {
            let mut state = self.state();
            let d = state.devices.get_mut(&device.0).unwrap();
            d.clients = d.clients.saturating_sub(1);
            d.clients == 0
        };
        if flipped {
            self.notify(ListenTarget::DeviceRunning(device));
        }
    }

    pub fn set_default_output(&self, device: Option<DeviceId>) {
        self.state().default_output = device;
        self.notify(ListenTarget::DefaultOutput);
    }

    pub fn kill(&self, device: DeviceId) {
        self.edit(device, |info| info.alive = false);
        self.notify(ListenTarget::DeviceAlive(device));
    }

    pub fn revive(&self, device: DeviceId) {
        self.edit(device, |info| info.alive = true);
    }

    fn call(&self, call: Call) {
        self.state().calls.push(call);
    }
}

impl Hal for FakeHal {
    fn translate_uid(&self, uid: &str) -> Result<Option<DeviceId>, HalError> {
        self.call(Call::Translate(uid.to_string()));
        let block = self.hooks().block_translate.take();
        if let Some(latch) = block {
            latch.wait();
        }
        if let Some(error) = self.hooks().translate_error {
            return Err(error);
        }
        Ok(self.state().uids.get(uid).copied().map(DeviceId))
    }

    fn device_info(&self, device: DeviceId) -> Result<DeviceInfo, HalError> {
        self.call(Call::Info(device));
        if let Some(error) = self.hooks().info_error.get(&device.0) {
            return Err(*error);
        }
        self.state()
            .devices
            .get(&device.0)
            .map(|d| d.info.clone())
            .ok_or(HalError::Gone)
    }

    fn is_alive(&self, device: DeviceId) -> Result<bool, HalError> {
        self.call(Call::Alive(device));
        self.state()
            .devices
            .get(&device.0)
            .map(|d| d.info.alive)
            .ok_or(HalError::Gone)
    }

    fn is_running_somewhere(&self, device: DeviceId) -> Result<bool, HalError> {
        self.call(Call::Running(device));
        let value = {
            let state = self.state();
            let d = state.devices.get(&device.0).ok_or(HalError::Gone)?;
            // A hidden device runs while someone (the host) runs IO on it.
            d.clients > 0
                || state
                    .sessions
                    .values()
                    .any(|s| !s.stopped && s.device == device)
        };
        let hook = self.hooks().on_running.take();
        if let Some(mut hook) = hook {
            hook(device);
            self.hooks().on_running = Some(hook);
        }
        Ok(value)
    }

    fn default_output_device(&self) -> Result<Option<DeviceId>, HalError> {
        self.call(Call::DefaultOutput);
        Ok(self.state().default_output)
    }

    fn add_listener(
        &self,
        target: ListenTarget,
        notifier: Arc<Notifier>,
    ) -> Result<ListenerId, HalError> {
        let hook = self.hooks().on_add_listener.take();
        if let Some(mut hook) = hook {
            hook(target);
            self.hooks().on_add_listener = Some(hook);
        }
        let mut state = self.state();
        state.calls.push(Call::AddListener(target));
        let id = state.next_listener;
        state.next_listener += 1;
        state.listeners.insert(id, (target, notifier));
        Ok(ListenerId(id))
    }

    fn remove_listener(&self, listener: ListenerId) -> Result<(), HalError> {
        let mut state = self.state();
        let (target, _) = state.listeners.remove(&listener.0).ok_or(HalError::Gone)?;
        state.calls.push(Call::RemoveListener(target));
        Ok(())
    }

    fn flush_listeners(&self, timeout: Duration) -> bool {
        self.call(Call::Flush);
        // A real queue flush: wait for every notification still running, up to the timeout.
        let deadline = Instant::now() + timeout;
        while self.shared.pending_notifications.load(Ordering::SeqCst) > 0 {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        true
    }

    fn start_io(
        &self,
        device: DeviceId,
        callback: Arc<dyn IoCallback>,
    ) -> Result<Box<dyn IoSession>, HalError> {
        self.call(Call::StartIo(device));
        let block = self.hooks().block_start.take();
        if let Some(latch) = block {
            latch.wait();
        }
        if let Some(error) = self.hooks().start_error {
            return Err(error);
        }
        let mut state = self.state();
        let id = state.next_session;
        state.next_session += 1;
        state.started += 1;
        state.sessions.insert(
            id,
            Session {
                device,
                callback: Some(callback),
                stopped: false,
            },
        );
        Ok(Box::new(FakeSession {
            shared: self.shared.clone(),
            id,
            device,
        }))
    }
}

struct FakeSession {
    shared: Arc<Shared>,
    id: u64,
    device: DeviceId,
}

impl std::fmt::Debug for FakeSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeSession").field("id", &self.id).finish()
    }
}

impl IoSession for FakeSession {
    fn stop(&mut self) -> Result<(), HalError> {
        let delay = self.shared.hooks.lock().unwrap().stop_delay;
        self.shared.stopping.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(delay);
        self.shared.stopping.fetch_sub(1, Ordering::SeqCst);
        let mut state = self.shared.state.lock().unwrap();
        let session = state.sessions.get_mut(&self.id).unwrap();
        if !session.stopped {
            session.stopped = true;
            state.stopped += 1;
            state.calls.push(Call::StopIo(self.device));
        }
        Ok(())
    }
}

impl Drop for FakeSession {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().unwrap();
        state.dropped += 1;
        if let Some(session) = state.sessions.get_mut(&self.id) {
            session.callback = None;
        }
    }
}

// -- IO cycles --------------------------------------------------------------------------------

/// One buffer of a driven IO cycle, backed by 4-byte-aligned storage.
pub struct Buf {
    pub channels: u32,
    storage: Vec<f32>,
    pub byte_size: u32,
    pub null: bool,
    /// Bytes to skew the data pointer by (to make it misaligned).
    pub skew: usize,
}

impl Buf {
    pub fn new(channels: u32, samples: &[f32]) -> Self {
        let mut storage = samples.to_vec();
        // Spare room so a skewed pointer never reads past the allocation.
        storage.extend([0.0; 4]);
        Self {
            channels,
            storage,
            byte_size: (samples.len() * 4) as u32,
            null: false,
            skew: 0,
        }
    }

    /// A buffer pre-filled with a marker (so tests can see what was overwritten).
    pub fn marked(channels: u32, samples: usize) -> Self {
        Self::new(channels, &vec![7.0; samples])
    }

    pub fn null(channels: u32, byte_size: u32) -> Self {
        let mut buf = Self::new(channels, &[]);
        buf.null = true;
        buf.byte_size = byte_size;
        buf
    }

    pub fn samples(&self) -> Vec<f32> {
        self.storage[..self.byte_size as usize / 4].to_vec()
    }

    fn io(&mut self) -> IoBuffer {
        IoBuffer {
            channels: self.channels,
            byte_size: self.byte_size,
            data: if self.null {
                std::ptr::null_mut()
            } else {
                // SAFETY: skew is at most 3 bytes into storage with 4 spare floats.
                unsafe { self.storage.as_mut_ptr().cast::<u8>().add(self.skew) }.cast::<c_void>()
            },
        }
    }
}

/// Drive one IO cycle through `callback` on this thread.
pub fn run_cycle(callback: &dyn IoCallback, input: &mut [Buf], output: &mut [Buf]) {
    let inputs: Vec<IoBuffer> = input.iter_mut().map(Buf::io).collect();
    let outputs: Vec<IoBuffer> = output.iter_mut().map(Buf::io).collect();
    // SAFETY: every `IoBuffer` points into a `Buf` that is alive and exclusively borrowed for the
    // whole call, valid for its `byte_size` bytes (the only skewed pointers have a byte size the
    // callback rejects before any access).
    unsafe { callback.process(&inputs, &outputs) };
}

/// An interleaved stereo pattern: frame `i` is (`base + i`, `-(base + i)`), `frames` frames.
pub fn stereo_pattern(base: usize, frames: usize) -> Vec<f32> {
    (0..frames)
        .flat_map(|i| {
            let v = (base + i) as f32;
            [v, -v]
        })
        .collect()
}

/// Drive one IO cycle with hand-built buffer descriptors (to test overlap and other layouts the
/// `Buf` helper cannot express).
///
/// # Safety
///
/// Every non-null pointer in `input`/`output` is valid for its `byte_size` bytes for the call,
/// except where the callback is expected to reject the layout before touching them.
pub unsafe fn run_cycle_raw(callback: &dyn IoCallback, input: &[IoBuffer], output: &[IoBuffer]) {
    // SAFETY: forwarded from this function's contract.
    unsafe { callback.process(input, output) };
}
