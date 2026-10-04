//! Bounded fixture output. Route owns target admission; this module owns only its client stream.
use super::routing::{self, Route, Status};
use crate::fixture::{FixtureError, OwnToneState, SpeakersSelection, ToneId};
use pipewire::{self as pw, spa};
use std::{
    cell::Cell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, FixtureError>;
const RATE: usize = 48_000;
const FRAMES: usize = RATE * 2;
const STRIDE: usize = 8;
const RAMP: usize = RATE / 50;
const PEAK: f64 = 0.0316228;
const BLOCK_BYTES: usize = 16_384;
const OPEN: Duration = Duration::from_secs(2);
const STOP: Duration = Duration::from_millis(50);
static WORKERS: AtomicUsize = AtomicUsize::new(0);
struct Slot;
impl Slot {
    fn take() -> Result<Self> {
        WORKERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 4).then_some(n + 1)
            })
            .map_err(|_| FixtureError::Busy)?;
        Ok(Self)
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}
fn code(error: FixtureError) -> u8 {
    match error {
        FixtureError::OutputChanged => 3,
        FixtureError::UnsupportedFormat => 4,
        FixtureError::TimedOut => 5,
        _ => 2,
    }
}
fn reply(code: u8) -> Option<Result<()>> {
    match code {
        0 => None,
        1 => Some(Ok(())),
        3 => Some(Err(FixtureError::OutputChanged)),
        4 => Some(Err(FixtureError::UnsupportedFormat)),
        5 => Some(Err(FixtureError::TimedOut)),
        _ => Some(Err(FixtureError::OutputUnavailable)),
    }
}
struct Control {
    epoch: Instant,
    stop_ns: AtomicU64,
    stream_created: AtomicBool,
}
impl Control {
    fn silence(&self, status: &Status, at: Instant) {
        let nanos = at.saturating_duration_since(self.epoch).as_nanos();
        let encoded = u64::try_from(nanos).unwrap_or(u64::MAX).saturating_add(1);
        let _ = self
            .stop_ns
            .compare_exchange(0, encoded, Ordering::AcqRel, Ordering::Acquire);
        status.disabled.store(true, Ordering::Release);
    }
    fn stop_deadline(&self) -> Instant {
        let encoded = self.stop_ns.load(Ordering::Acquire);
        self.epoch
            .checked_add(Duration::from_nanos(encoded.saturating_sub(1)))
            .and_then(|at| at.checked_add(STOP))
            .unwrap_or(self.epoch)
    }
}
struct Finish {
    status: Arc<Status>,
    control: Arc<Control>,
}
impl Drop for Finish {
    fn drop(&mut self) {
        self.status.disabled.store(true, Ordering::Release);
        if !self.control.stream_created.load(Ordering::Acquire) {
            self.status.removed.store(true, Ordering::Release);
        }
    }
}
struct Active {
    id: ToneId,
    output: SpeakersSelection,
    status: Arc<Status>,
    deadline: Instant,
    control: Arc<Control>,
}
pub(super) struct Output {
    runtime: Option<PathBuf>,
    forbidden_override: bool,
    active: Option<Active>,
    rejected: Option<(ToneId, SpeakersSelection, FixtureError)>,
}
impl Output {
    pub(super) fn new(runtime: Option<PathBuf>, forbidden_override: bool) -> Self {
        Self {
            runtime,
            forbidden_override,
            active: None,
            rejected: None,
        }
    }
    fn reject(
        &mut self,
        id: ToneId,
        output: &SpeakersSelection,
        error: FixtureError,
    ) -> Option<Result<()>> {
        self.rejected = Some((id, output.clone(), error));
        Some(Err(error))
    }
    pub(super) fn play(&mut self, id: ToneId, output: &SpeakersSelection) -> Option<Result<()>> {
        if id.0 == 0 || output.device_key != format!("crosspane.{}.speaker", output.peer) {
            return Some(Err(FixtureError::NotOwned));
        }
        if let Some((previous, selected, error)) = &self.rejected {
            if *previous == id {
                return Some(Err(if selected == output {
                    *error
                } else {
                    FixtureError::NotOwned
                }));
            }
            if id < *previous {
                return Some(Err(FixtureError::NotOwned));
            }
        }
        if let Some(active) = &self.active {
            if active.id == id {
                if active.output != *output {
                    return Some(Err(FixtureError::NotOwned));
                }
                if active.status.reply.load(Ordering::Acquire) == 0
                    && Instant::now() >= active.deadline
                {
                    active.control.silence(&active.status, active.deadline);
                    active.status.fail(5);
                }
                return reply(active.status.reply.load(Ordering::Acquire));
            }
            if id <= active.id {
                return Some(Err(FixtureError::NotOwned));
            }
            if !active.status.removed.load(Ordering::Acquire) {
                return self.reject(id, output, FixtureError::Busy);
            }
        }
        if self.forbidden_override {
            return self.reject(id, output, FixtureError::Refused);
        }
        let Some(runtime) = self.runtime.clone() else {
            return self.reject(id, output, FixtureError::OutputUnavailable);
        };
        let slot = match Slot::take() {
            Ok(slot) => slot,
            Err(error) => return self.reject(id, output, error),
        };
        let status = Arc::new(Status::default());
        let epoch = Instant::now();
        let deadline = epoch + OPEN;
        let control = Arc::new(Control {
            epoch,
            stop_ns: AtomicU64::new(0),
            stream_created: AtomicBool::new(false),
        });
        self.rejected = None;
        self.active = Some(Active {
            id,
            output: output.clone(),
            status: status.clone(),
            deadline,
            control: control.clone(),
        });
        let output = output.clone();
        let worker = status.clone();
        if thread::Builder::new()
            .name("tutorial-tone".into())
            .spawn(move || {
                let _slot = slot;
                let _finish = Finish {
                    status: worker.clone(),
                    control: control.clone(),
                };
                let result = native(&runtime, &output, worker.clone(), control, deadline);
                if let Err(error) = result {
                    worker.fail(code(error));
                }
                worker.disabled.store(true, Ordering::Release);
            })
            .is_err()
        {
            status.fail(2);
            status.removed.store(true, Ordering::Release);
        }
        reply(status.reply.load(Ordering::Acquire))
    }
    pub(super) fn stop(&mut self, id: ToneId) -> Option<Result<()>> {
        let Some(active) = &mut self.active else {
            return Some(Err(FixtureError::NotOwned));
        };
        if active.id != id {
            return Some(Err(FixtureError::NotOwned));
        }
        active.control.silence(&active.status, Instant::now());
        active.status.fail(2);
        let deadline = active.control.stop_deadline();
        if active.status.removed.load(Ordering::Acquire) {
            Some(Ok(()))
        } else if Instant::now() >= deadline {
            Some(Err(FixtureError::CleanupFailed))
        } else {
            None
        }
    }
    pub(super) fn state(&self) -> OwnToneState {
        match &self.active {
            None => OwnToneState::Stopped,
            Some(active) if active.status.removed.load(Ordering::Acquire) => OwnToneState::Stopped,
            Some(active)
                if active.status.disabled.load(Ordering::Acquire)
                    || !active.status.rendered.load(Ordering::Acquire) =>
            {
                OwnToneState::StopUnconfirmed { tone: active.id }
            }
            Some(active) => OwnToneState::Running { tone: active.id },
        }
    }
}
impl Drop for Output {
    fn drop(&mut self) {
        if let Some(active) = &self.active {
            active.control.silence(&active.status, Instant::now());
            active.status.fail(2);
        }
    }
}
struct Pcm {
    bytes: Box<[u8]>,
    cursor: usize,
    started: Option<Instant>,
}
impl Pcm {
    fn new() -> Self {
        let mut bytes = Vec::with_capacity(FRAMES * STRIDE);
        for frame in 0..FRAMES {
            let gain = (frame.min(FRAMES - 1 - frame) as f64 / RAMP as f64).min(1.0);
            let sample =
                (PEAK * gain * (std::f64::consts::TAU * 1000.0 * frame as f64 / RATE as f64).sin())
                    as f32;
            // Round the literal peak down so F32 encoding never exceeds its decimal bound.
            let bound = f32::from_bits((PEAK as f32).to_bits() - 1);
            let sample = sample.clamp(-bound, bound).to_le_bytes();
            bytes.extend_from_slice(&sample);
            bytes.extend_from_slice(&sample);
        }
        Self {
            bytes: bytes.into_boxed_slice(),
            cursor: 0,
            started: None,
        }
    }
    fn fill(&mut self, out: &mut [u8], now: Instant, allowed: bool) -> (usize, bool) {
        out.fill(0);
        let n = (out.len() / STRIDE * STRIDE).min(self.bytes.len() - self.cursor);
        if !allowed || n == 0 {
            return (n, false);
        }
        let start = *self.started.get_or_insert(now);
        if now.saturating_duration_since(start) >= OPEN {
            return (n, false);
        }
        out[..n].copy_from_slice(&self.bytes[self.cursor..self.cursor + n]);
        self.cursor += n;
        (n, out[..n].iter().any(|byte| *byte != 0))
    }
}
struct Playback {
    pcm: Pcm,
    status: Arc<Status>,
    ready: Arc<AtomicBool>,
    format: Arc<AtomicBool>,
    streaming: Arc<AtomicBool>,
    started: Arc<AtomicU64>,
    epoch: Instant,
    deadline: Instant,
}
fn exact_format(info: &spa::param::audio::AudioInfoRaw) -> bool {
    info.format() == spa::param::audio::AudioFormat::F32LE
        && info.rate() == RATE as u32
        && info.channels() == 2
        && !info
            .flags()
            .contains(spa::param::audio::AudioInfoRawFlags::UNPOSITIONED)
        && info.position()[..2]
            == [
                spa::sys::SPA_AUDIO_CHANNEL_FL,
                spa::sys::SPA_AUDIO_CHANNEL_FR,
            ]
}
fn audio_format() -> spa::param::audio::AudioInfoRaw {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(RATE as u32);
    info.set_channels(2);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    info
}
impl Playback {
    fn format_changed(&self, pod: Option<&spa::pod::Pod>) {
        #[cfg(test)]
        test_support::format_probe(pod);
        let mut info = spa::param::audio::AudioInfoRaw::new();
        let valid = pod.is_some_and(|pod| {
            spa::param::format_utils::parse_format(pod).is_ok_and(|media| {
                media
                    == (
                        spa::param::format::MediaType::Audio,
                        spa::param::format::MediaSubtype::Raw,
                    )
            }) && info.parse(pod).is_ok()
        }) && exact_format(&info);
        self.format.store(valid, Ordering::Release);
        if !valid {
            self.status.fail(4);
        }
    }
    fn state_changed(&self, old: pw::stream::StreamState, new: pw::stream::StreamState) {
        let running = matches!(new, pw::stream::StreamState::Streaming);
        self.streaming.store(running, Ordering::Release);
        if (!running && self.status.rendered.load(Ordering::Acquire))
            || matches!(new, pw::stream::StreamState::Error(_))
            || (matches!(new, pw::stream::StreamState::Unconnected)
                && !matches!(old, pw::stream::StreamState::Unconnected))
        {
            self.status.fail(2);
        }
    }
    fn fill(&mut self, bytes: &mut [u8], now: Instant) -> (usize, bool) {
        #[cfg(test)]
        test_support::buffer_probe(1, bytes.len());
        let capacity = bytes.len().min(BLOCK_BYTES);
        let bytes = &mut bytes[..capacity];
        if capacity < STRIDE {
            #[cfg(test)]
            test_support::reason(7);
            bytes.fill(0);
            self.status.fail(4);
            return (0, false);
        }
        if self.started.load(Ordering::Acquire) == 0 && now >= self.deadline {
            self.status.fail(5);
        }
        let allowed = self.ready.load(Ordering::Acquire)
            && self.format.load(Ordering::Acquire)
            && self.streaming.load(Ordering::Acquire)
            && !self.status.disabled.load(Ordering::Acquire);
        let (size, mut nonzero) = self.pcm.fill(bytes, now, allowed);
        if self.started.load(Ordering::Acquire) == 0 && Instant::now() >= self.deadline {
            self.status.fail(5);
        }
        if self.status.disabled.load(Ordering::Acquire) {
            bytes.fill(0);
            nonzero = false;
        }
        (size, nonzero)
    }
    fn queue(&self, size: usize, nonzero: bool, queue: impl FnOnce(usize)) {
        // Recheck at release: cancellation can follow preparation of non-silent bytes.
        let disabled = self.status.disabled.load(Ordering::Acquire);
        queue(if disabled { 0 } else { size });
        self.submitted(nonzero && !disabled, Instant::now());
    }
    fn submitted(&self, nonzero: bool, now: Instant) {
        if self.started.load(Ordering::Acquire) == 0 && now >= self.deadline {
            self.status.fail(5);
        }
        if nonzero && !self.status.disabled.load(Ordering::Acquire) {
            let elapsed = now.saturating_duration_since(self.epoch).as_nanos();
            if let Ok(elapsed) = u64::try_from(elapsed) {
                let _ = self.started.compare_exchange(
                    0,
                    elapsed.saturating_add(1),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            } else {
                self.status.fail(5);
                return;
            }
            self.status.rendered.store(true, Ordering::Release);
            let _ = self
                .status
                .reply
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
        }
    }
}
fn process(stream: &pw::stream::Stream, playback: &mut Playback) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    #[cfg(test)]
    test_support::buffer_probe(buffer.datas_mut().len(), 0);
    if buffer.datas_mut().len() != 1 {
        playback.status.fail(4);
        // Never queue malformed multi-plane data containing unvalidated bytes.
        // The worker's immediate stream destruction releases this borrowed native buffer.
        std::mem::forget(buffer);
        return;
    }
    let data = &mut buffer.datas_mut()[0];
    let (size, nonzero) = match data.data() {
        Some(bytes) => playback.fill(bytes, Instant::now()),
        None => {
            #[cfg(test)]
            test_support::reason(6);
            playback.status.fail(4);
            (0, false)
        }
    };
    let chunk = data.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = STRIDE as i32;
    playback.queue(size, nonzero, |size| {
        *buffer.datas_mut()[0].chunk_mut().size_mut() = size as u32;
        drop(buffer);
    });
}
fn tick(loop_: &pw::main_loop::MainLoopRc, last_dispatch: &mut Instant) -> Result<()> {
    *last_dispatch = Instant::now();
    if loop_
        .loop_()
        .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(2)))
        < 0
    {
        Err(FixtureError::OutputUnavailable)
    } else {
        Ok(())
    }
}
fn check(status: &Status, deadline: Instant) -> Result<()> {
    routing::remaining(deadline)?;
    if status.disabled.load(Ordering::Acquire) {
        Err(FixtureError::OutputUnavailable)
    } else {
        Ok(())
    }
}
fn native(
    runtime: &Path,
    output: &SpeakersSelection,
    status: Arc<Status>,
    control: Arc<Control>,
    deadline: Instant,
) -> Result<()> {
    let mut last_dispatch = control.epoch;
    check(&status, deadline)?;
    #[cfg(test)]
    test_support::set_phase(1);
    let fd = routing::socket(runtime, deadline)?;
    check(&status, deadline)?;
    #[cfg(test)]
    test_support::verify_fd(&fd)?;
    #[cfg(test)]
    test_support::set_phase(2);
    pw::init();
    let loop_ =
        pw::main_loop::MainLoopRc::new(None).map_err(|_| FixtureError::OutputUnavailable)?;
    let context =
        pw::context::ContextRc::new(&loop_, None).map_err(|_| FixtureError::OutputUnavailable)?;
    check(&status, deadline)?;
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|_| FixtureError::OutputUnavailable)?;
    let done = Rc::new(Cell::new(None));
    let arrived = done.clone();
    let failed = status.clone();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == 0 {
                arrived.set(Some(seq));
            }
        })
        .error(move |_, _, _, _| failed.fail(2))
        .register();
    #[cfg(test)]
    test_support::set_phase(3);
    let mut route = Route::new(core.clone(), output, status.clone())?;
    for _ in 0..2 {
        let seq = core.sync(0).map_err(|_| FixtureError::OutputUnavailable)?;
        while done.get() != Some(seq) {
            check(&status, deadline)?;
            tick(&loop_, &mut last_dispatch)?;
        }
    }
    #[cfg(test)]
    test_support::set_phase(4);
    route.pin_sink()?;
    check(&status, deadline)?;
    let ready = Arc::new(AtomicBool::new(false));
    let format = Arc::new(AtomicBool::new(false));
    let streaming = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicU64::new(0));
    let epoch = control.epoch;
    let stream = pw::stream::StreamBox::new(
        &core,
        "Crosspane practice output",
        pw::properties::properties! {
            "media.type" => "Audio", "media.category" => "Playback",
            "media.role" => "Test", "node.name" => "crosspane.practice.tone",
            "node.virtual" => "true", "node.dont-fallback" => "true",
            "node.dont-move" => "true", "node.dont-reconnect" => "true",
            "adapter.auto-port-config" => "{ mode=dsp monitor=false position=preserve }",
        },
    )
    .map_err(|_| FixtureError::OutputUnavailable)?;
    control.stream_created.store(true, Ordering::Release);
    let playback = Playback {
        pcm: Pcm::new(),
        status: status.clone(),
        ready: ready.clone(),
        format: format.clone(),
        streaming: streaming.clone(),
        started: started.clone(),
        epoch,
        deadline,
    };
    let listener = stream
        .add_local_listener_with_user_data(playback)
        .param_changed(|_, playback, id, pod| {
            if id == spa::sys::SPA_PARAM_Format {
                playback.format_changed(pod);
            }
        })
        .state_changed(|_, playback, old, new| playback.state_changed(old, new))
        .process(process)
        .register()
        .map_err(|_| FixtureError::OutputUnavailable)?;
    let pod = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: audio_format().into(),
        }),
    )
    .map_err(|_| FixtureError::UnsupportedFormat)?
    .0
    .into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&pod).ok_or(FixtureError::UnsupportedFormat)?];
    check(&status, deadline)?;
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::DONT_RECONNECT,
            &mut params,
        )
        .map_err(|_| FixtureError::OutputUnavailable)?;
    #[cfg(test)]
    test_support::set_phase(5);
    let run = (|| {
        let mut attached = false;
        while !route.established()
            || !format.load(Ordering::Acquire)
            || !streaming.load(Ordering::Acquire)
        {
            check(&status, deadline)?;
            if !attached && let Some(result) = route.attach(&stream) {
                result?;
                attached = true;
            }
            tick(&loop_, &mut last_dispatch)?;
        }
        check(&status, deadline)?;
        #[cfg(test)]
        test_support::set_phase(6);
        ready.store(true, Ordering::Release);
        loop {
            if status.disabled.load(Ordering::Acquire) {
                return Ok(());
            }
            let first = started.load(Ordering::Acquire);
            if first == 0 {
                check(&status, deadline)?;
            } else if epoch + Duration::from_nanos(first - 1) + OPEN <= Instant::now() {
                return Ok(());
            }
            tick(&loop_, &mut last_dispatch)?;
        }
    })();
    status.disabled.store(true, Ordering::Release);
    ready.store(false, Ordering::Release);
    #[cfg(test)]
    test_support::set_phase(7);
    // Cancellation drops queued output; it never waits for a drain or restarts a stream.
    if control.stop_ns.load(Ordering::Acquire) == 0 {
        control.silence(&status, last_dispatch);
    }
    let stop = control.stop_deadline().min(last_dispatch + STOP);
    let cleanup = stream
        .set_active(false)
        .and_then(|_| stream.flush(false))
        .and_then(|_| stream.disconnect())
        .map_err(|_| FixtureError::CleanupFailed);
    drop(listener);
    drop(stream);
    drop(route);
    let flushed = (|| {
        let seq = core.sync(0).map_err(|_| FixtureError::CleanupFailed)?;
        while done.get() != Some(seq) {
            routing::remaining(stop).map_err(|_| FixtureError::CleanupFailed)?;
            tick(&loop_, &mut last_dispatch).map_err(|_| FixtureError::CleanupFailed)?;
        }
        Ok(())
    })();
    if cleanup.is_ok() && flushed.is_ok() {
        status.removed.store(true, Ordering::Release);
    }
    #[cfg(test)]
    test_support::set_phase(8);
    run.and(cleanup).and(flushed)
}

#[cfg(test)]
pub(super) mod test_support {
    use super::*;
    use std::{os::fd::OwnedFd, sync::OnceLock};
    type Guard = Box<dyn Fn(&OwnedFd) -> Result<()> + Send + Sync>;
    static GUARD: OnceLock<Guard> = OnceLock::new();
    static PHASE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
    pub(crate) fn set_phase(value: u8) {
        PHASE.store(value, Ordering::Release);
    }
    pub(crate) fn phase() -> u8 {
        PHASE.load(Ordering::Acquire)
    }
    static REASON: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
    static FACTS: [std::sync::atomic::AtomicU32; 7] =
        [const { std::sync::atomic::AtomicU32::new(0) }; 7];
    static CAPACITY: AtomicUsize = AtomicUsize::new(0);
    static PLANES: AtomicUsize = AtomicUsize::new(0);
    pub(crate) fn reason(value: u8) {
        let _ = REASON.compare_exchange(0, value, Ordering::AcqRel, Ordering::Acquire);
    }
    pub(crate) fn buffer_probe(planes: usize, capacity: usize) {
        PLANES.store(planes, Ordering::Release);
        if capacity > 0 {
            CAPACITY.store(capacity, Ordering::Release);
        }
        if planes != 1 {
            reason(5);
        }
    }
    pub(crate) fn format_probe(pod: Option<&spa::pod::Pod>) {
        let Some(pod) = pod else {
            reason(1);
            return;
        };
        let raw = spa::param::format_utils::parse_format(pod).is_ok_and(|media| {
            media
                == (
                    spa::param::format::MediaType::Audio,
                    spa::param::format::MediaSubtype::Raw,
                )
        });
        if !raw {
            reason(2);
            return;
        }
        let mut info = spa::param::audio::AudioInfoRaw::new();
        if info.parse(pod).is_err() {
            reason(3);
            return;
        }
        let values = [
            u32::from(info.format() == spa::param::audio::AudioFormat::F32LE),
            info.rate(),
            info.channels(),
            info.flags().bits(),
            info.position()[0],
            info.position()[1],
            u32::from(raw),
        ];
        for (field, value) in FACTS.iter().zip(values) {
            field.store(value, Ordering::Release);
        }
        if !exact_format(&info) {
            reason(4);
        }
    }
    pub(crate) fn diagnostic() -> (u8, [u32; 7], usize, usize) {
        (
            REASON.load(Ordering::Acquire),
            std::array::from_fn(|i| FACTS[i].load(Ordering::Acquire)),
            PLANES.load(Ordering::Acquire),
            CAPACITY.load(Ordering::Acquire),
        )
    }
    pub(crate) fn install_guard(guard: Guard) {
        assert!(
            GUARD.set(guard).is_ok(),
            "only one owned-server proof per test client"
        );
    }
    pub(crate) fn verify_fd(fd: &OwnedFd) -> Result<()> {
        GUARD.get().ok_or(FixtureError::Refused)?(fd)
    }
    pub(crate) struct Model {
        output: Output,
        playback: Playback,
    }
    impl Model {
        pub(crate) fn new(id: ToneId, selected: &SpeakersSelection, now: Instant) -> Self {
            let status = Arc::new(Status::default());
            let control = Arc::new(Control {
                epoch: now,
                stop_ns: AtomicU64::new(0),
                stream_created: AtomicBool::new(true),
            });
            let mut output = Output::new(None, false);
            output.active = Some(Active {
                id,
                output: selected.clone(),
                status: status.clone(),
                deadline: now + OPEN,
                control,
            });
            Self {
                output,
                playback: Playback {
                    pcm: Pcm::new(),
                    status,
                    ready: Arc::new(AtomicBool::new(false)),
                    format: Arc::new(AtomicBool::new(false)),
                    streaming: Arc::new(AtomicBool::new(false)),
                    started: Arc::new(AtomicU64::new(0)),
                    epoch: now,
                    deadline: now + OPEN,
                },
            }
        }
        pub(crate) fn gates(&self, route: bool, format: bool, streaming: bool) {
            self.playback.ready.store(route, Ordering::Release);
            self.playback.format.store(format, Ordering::Release);
            self.playback.streaming.store(streaming, Ordering::Release);
        }
        pub(crate) fn bytes(&self) -> &[u8] {
            &self.playback.pcm.bytes
        }
        pub(crate) fn cursor(&self) -> usize {
            self.playback.pcm.cursor
        }
        pub(crate) fn fill(&mut self, bytes: &mut [u8], at: Instant) -> (usize, bool) {
            self.playback.fill(bytes, at)
        }
        pub(crate) fn submitted(&self, nonzero: bool, at: Instant) {
            self.playback.submitted(nonzero, at);
        }
        pub(crate) fn queued_bytes(&self, bytes: &[u8], size: usize, nonzero: bool) -> Vec<u8> {
            let mut queued = Vec::new();
            self.playback.queue(size, nonzero, |size| {
                queued.extend_from_slice(&bytes[..size]);
            });
            queued
        }
        pub(crate) fn play(
            &mut self,
            id: ToneId,
            selected: &SpeakersSelection,
        ) -> Option<Result<()>> {
            self.output.play(id, selected)
        }
        pub(crate) fn stop(&mut self, id: ToneId) -> Option<Result<()>> {
            self.output.stop(id)
        }
        pub(crate) fn state(&self) -> OwnToneState {
            self.output.state()
        }
        pub(crate) fn fail(&self, code: u8) {
            self.playback.status.fail(code);
        }
        pub(crate) fn disabled(&self) -> bool {
            self.playback.status.disabled.load(Ordering::Acquire)
        }
        pub(crate) fn reply(&self) -> u8 {
            self.playback.status.reply.load(Ordering::Acquire)
        }
        pub(crate) fn removed(&self) {
            self.playback.status.removed.store(true, Ordering::Release);
        }
        pub(crate) fn cancel_at(&self, at: Instant) {
            let active = self.output.active.as_ref().unwrap();
            active.control.silence(&active.status, at);
        }
        pub(crate) fn stop_deadline(&self) -> Instant {
            self.output.active.as_ref().unwrap().control.stop_deadline()
        }
        pub(crate) fn format(&self, info: Option<spa::param::audio::AudioInfoRaw>, media: u32) {
            let pod = info.map(|info| {
                let mut properties: Vec<spa::pod::Property> = info.into();
                if media != spa::sys::SPA_MEDIA_TYPE_audio {
                    properties[0] = spa::pod::Property {
                        key: spa::sys::SPA_FORMAT_mediaType,
                        flags: spa::pod::PropertyFlags::empty(),
                        value: spa::pod::Value::Id(spa::utils::Id(media)),
                    };
                }
                spa::pod::serialize::PodSerializer::serialize(
                    std::io::Cursor::new(Vec::new()),
                    &spa::pod::Value::Object(spa::pod::Object {
                        type_: spa::sys::SPA_TYPE_OBJECT_Format,
                        id: spa::sys::SPA_PARAM_Format,
                        properties,
                    }),
                )
                .unwrap()
                .0
                .into_inner()
            });
            self.playback
                .format_changed(pod.as_deref().and_then(spa::pod::Pod::from_bytes));
        }
        pub(crate) fn stream_state(
            &self,
            old: pw::stream::StreamState,
            new: pw::stream::StreamState,
        ) {
            self.playback.state_changed(old, new);
        }
        pub(crate) fn status(&self) -> Arc<Status> {
            self.playback.status.clone()
        }
        pub(crate) fn drop_output(self) -> Arc<Status> {
            let status = self.status();
            drop(self.output);
            status
        }
    }
    pub(crate) fn fixed_format() -> spa::param::audio::AudioInfoRaw {
        audio_format()
    }
    pub(crate) fn lease() -> Result<impl Drop> {
        Slot::take()
    }
    pub(crate) fn slots() -> usize {
        WORKERS.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{self as tone_test, Model as ToneModel};
    use super::*;
    use std::os::unix::net::UnixStream;
    fn speakers() -> SpeakersSelection {
        let peer = crosspane_types::id::NodeId([0x31; 32]);
        SpeakersSelection {
            peer,
            device_key: format!("crosspane.{peer}.speaker"),
        }
    }
    fn tone_model(now: Instant) -> ToneModel {
        ToneModel::new(ToneId(1), &speakers(), now)
    }
    #[test]
    fn b3b_fixed_pcm_is_stereo_48k_one_khz_bounded_ramped_and_peak_limited() {
        let model = tone_model(Instant::now());
        let bytes = model.bytes();
        assert_eq!(bytes.len(), 96_000 * 8);
        let samples: Vec<_> = bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|frame| {
                assert_eq!(&frame[..4], &frame[4..]);
                f32::from_le_bytes(frame[..4].try_into().unwrap())
            })
            .collect();
        assert_eq!(samples[0], 0.0);
        assert_eq!(samples[95_999], 0.0);
        assert!(
            samples
                .iter()
                .all(|v| v.is_finite() && f64::from(v.abs()) <= 0.0316228)
        );
        assert!(samples[12].abs() < samples[972].abs());
        assert!(samples[95_988].abs() < samples[95_028].abs());
        for frame in 960..94_992 {
            assert!((samples[frame] - samples[frame + 48]).abs() < 1e-7);
        }
    }
    #[test]
    fn b3b_pending_link_format_and_streaming_are_silent_without_advancing_pcm() {
        for gates in [
            (false, true, true),
            (true, false, true),
            (true, true, false),
        ] {
            let now = Instant::now();
            let mut model = tone_model(now);
            model.gates(gates.0, gates.1, gates.2);
            let mut bytes = [255; 3840];
            let (size, nonzero) = model.fill(&mut bytes, now);
            assert_eq!(size, bytes.len());
            assert!(!nonzero && bytes.iter().all(|b| *b == 0));
            assert_eq!(model.cursor(), 0);
            assert_eq!(
                model.reply(),
                0,
                "an open/link acknowledgement is not sample evidence"
            );
        }
    }
    #[test]
    fn b3b_first_nonsilent_submission_only_is_local_running_evidence() {
        let now = Instant::now();
        let mut model = tone_model(now);
        model.gates(true, true, true);
        assert_eq!(
            model.state(),
            OwnToneState::StopUnconfirmed { tone: ToneId(1) }
        );
        let mut bytes = [0; 3840];
        let (_, nonzero) = model.fill(&mut bytes, now);
        assert!(nonzero);
        assert_eq!(model.reply(), 0, "preparing bytes is not submission");
        model.submitted(nonzero, now);
        assert_eq!(model.reply(), 1);
        assert_eq!(model.state(), OwnToneState::Running { tone: ToneId(1) });
        model.removed();
        assert_eq!(model.state(), OwnToneState::Stopped);
    }
    #[test]
    fn b3b_pcm_is_silent_at_two_seconds_and_after_96000_frames() {
        let now = Instant::now();
        let mut model = tone_model(now);
        model.gates(true, true, true);
        let mut bytes = [0; 3840];
        let (_, first) = model.fill(&mut bytes, now);
        model.submitted(first, now);
        for _ in 1..200 {
            model.fill(&mut bytes, now + Duration::from_millis(1));
        }
        assert_eq!(model.cursor(), 96_000 * 8);
        assert!(!model.fill(&mut bytes, now + Duration::from_millis(2)).1);
        assert!(bytes.iter().all(|b| *b == 0));
        let mut fresh = tone_model(now);
        fresh.gates(true, true, true);
        let (_, first) = fresh.fill(&mut bytes, now);
        fresh.submitted(first, now);
        assert!(!fresh.fill(&mut bytes, now + Duration::from_secs(2)).1);
        assert!(bytes.iter().all(|b| *b == 0));
    }
    #[test]
    fn b3b_late_first_callback_cannot_succeed_when_polling_is_suspended() {
        let now = Instant::now();
        let mut model = tone_model(now);
        model.gates(true, true, true);
        let mut bytes = [255; 3840];
        assert!(!model.fill(&mut bytes, now + Duration::from_secs(2)).1);
        model.submitted(true, now + Duration::from_secs(2));
        assert_eq!(model.reply(), 5);
        assert!(model.disabled());
        assert!(bytes.iter().all(|b| *b == 0));
        model.removed();
        assert_eq!(
            model.play(ToneId(1), &speakers()),
            Some(Err(FixtureError::TimedOut))
        );
    }
    #[test]
    fn b3b_stalled_worker_open_deadline_disables_and_late_completion_is_not_success() {
        let now = Instant::now() - Duration::from_secs(3);
        let mut model = tone_model(now);
        assert_eq!(
            model.play(ToneId(1), &speakers()),
            Some(Err(FixtureError::TimedOut))
        );
        model.gates(true, true, true);
        let mut bytes = [255; 3840];
        assert!(!model.fill(&mut bytes, Instant::now()).1);
        model.submitted(true, Instant::now());
        assert_eq!(model.reply(), 5);
        assert!(model.disabled());
        assert!(matches!(
            model.state(),
            OwnToneState::StopUnconfirmed { .. }
        ));
    }
    #[test]
    fn b3b_cancel_between_prepare_and_submit_silences_without_start() {
        let now = Instant::now();
        let mut model = tone_model(now);
        model.gates(true, true, true);
        let mut bytes = [0; 3840];
        let (size, nonzero) = model.fill(&mut bytes, now);
        assert!(nonzero);
        assert!(model.stop(ToneId(1)).is_none());
        assert!(model.disabled());
        let queued = model.queued_bytes(&bytes, size, nonzero);
        assert!(
            queued.iter().all(|byte| *byte == 0),
            "cancelled PCM was queued"
        );
        assert_ne!(model.reply(), 1);
        assert!(!model.fill(&mut bytes, now).1);
        assert!(bytes.iter().all(|b| *b == 0));
    }
    #[test]
    fn b3b_queue_boundary_preserves_admitted_uncancelled_chunk() {
        let now = Instant::now();
        let mut model = tone_model(now);
        model.gates(true, true, true);
        let mut bytes = [0; 3840];
        let (size, nonzero) = model.fill(&mut bytes, now);
        let queued = model.queued_bytes(&bytes, size, nonzero);
        assert_eq!(queued, bytes[..size]);
        assert!(queued.iter().any(|byte| *byte != 0));
        assert_eq!(model.reply(), 1);
    }
    #[test]
    fn b3b_stop_has_one_50ms_deadline_and_unknown_cleanup_never_claims_stopped() {
        let now = Instant::now() - Duration::from_millis(100);
        let mut model = tone_model(now);
        let at = now + Duration::from_millis(10);
        model.cancel_at(at);
        let deadline = model.stop_deadline();
        assert_eq!(
            model.stop(ToneId(1)),
            Some(Err(FixtureError::CleanupFailed))
        );
        assert_eq!(model.stop_deadline(), deadline);
        assert_eq!(
            model.state(),
            OwnToneState::StopUnconfirmed { tone: ToneId(1) }
        );
        assert_eq!(model.stop(ToneId(2)), Some(Err(FixtureError::NotOwned)));
        model.removed();
        assert_eq!(model.stop(ToneId(1)), Some(Ok(())));
        assert_eq!(model.state(), OwnToneState::Stopped);
    }
    #[test]
    fn b3b_failure_latches_through_good_format_late_render_and_natural_cleanup() {
        for (code, expected) in [
            (2, FixtureError::OutputUnavailable),
            (3, FixtureError::OutputChanged),
            (4, FixtureError::UnsupportedFormat),
            (5, FixtureError::TimedOut),
        ] {
            let now = Instant::now();
            let mut model = tone_model(now);
            model.fail(code);
            model.format(
                Some(tone_test::fixed_format()),
                pw::spa::sys::SPA_MEDIA_TYPE_audio,
            );
            model.gates(true, true, true);
            let mut bytes = [255; 3840];
            assert!(!model.fill(&mut bytes, now).1);
            model.submitted(true, now);
            model.removed();
            assert_eq!(model.play(ToneId(1), &speakers()), Some(Err(expected)));
            assert!(model.disabled());
            assert!(bytes.iter().all(|b| *b == 0));
        }
    }
    #[test]
    fn b3b_format_requires_raw_audio_f32le_48k_stereo_flfr_and_never_recovers() {
        for variant in 0..7 {
            let model = tone_model(Instant::now());
            let mut info = tone_test::fixed_format();
            match variant {
                0 => info.set_format(pw::spa::param::audio::AudioFormat::S16LE),
                1 => info.set_rate(44100),
                2 => info.set_channels(1),
                3 => {
                    let mut positions = [0; pw::spa::param::audio::MAX_CHANNELS];
                    positions[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
                    positions[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
                    info.set_position(positions);
                }
                _ => (),
            }
            model.format(
                if variant == 5 { None } else { Some(info) },
                if variant == 4 {
                    pw::spa::sys::SPA_MEDIA_TYPE_video
                } else {
                    pw::spa::sys::SPA_MEDIA_TYPE_audio
                },
            );
            if variant == 6 {
                assert!(!model.disabled(), "exact format alone is admitted");
            } else {
                assert_eq!(model.reply(), 4);
                model.format(
                    Some(tone_test::fixed_format()),
                    pw::spa::sys::SPA_MEDIA_TYPE_audio,
                );
                assert!(model.disabled(), "good later format cannot clear a failure");
            }
        }
    }
    #[test]
    fn b3b_later_stream_pause_or_error_disables_and_never_restarts() {
        for new in [
            pw::stream::StreamState::Paused,
            pw::stream::StreamState::Error("test-owned".into()),
        ] {
            let now = Instant::now();
            let mut model = tone_model(now);
            model.gates(true, true, true);
            model.submitted(true, now);
            model.stream_state(pw::stream::StreamState::Streaming, new);
            assert!(model.disabled());
            model.stream_state(
                pw::stream::StreamState::Paused,
                pw::stream::StreamState::Streaming,
            );
            let mut bytes = [255; 3840];
            assert!(!model.fill(&mut bytes, now).1);
            assert!(bytes.iter().all(|b| *b == 0));
        }
    }
    #[test]
    fn b3b_completed_busy_is_cached_and_wrong_selector_cannot_cancel_owned_tone() {
        let now = Instant::now();
        let mut model = tone_model(now);
        assert_eq!(
            model.play(ToneId(2), &speakers()),
            Some(Err(FixtureError::Busy))
        );
        model.removed();
        assert_eq!(
            model.play(ToneId(2), &speakers()),
            Some(Err(FixtureError::Busy)),
            "no implicit retry"
        );
        let mut other = speakers();
        other.peer = crosspane_types::id::NodeId([0x32; 32]);
        other.device_key = format!("crosspane.{}.speaker", other.peer);
        assert_eq!(
            model.play(ToneId(2), &other),
            Some(Err(FixtureError::NotOwned))
        );
        assert_eq!(
            model.play(ToneId(3), &speakers()),
            Some(Err(FixtureError::OutputUnavailable))
        );
        assert_eq!(
            model.play(ToneId(3), &speakers()),
            Some(Err(FixtureError::OutputUnavailable))
        );
    }
    #[test]
    fn b3b_drop_disables_owned_tone_and_does_not_fabricate_cleanup() {
        let model = tone_model(Instant::now());
        let status = model.drop_output();
        assert!(status.disabled.load(Ordering::Acquire));
        assert!(!status.removed.load(Ordering::Acquire));
    }
    #[test]
    fn b3b_callback_large_capacity_queues_only_a_bounded_chunk() {
        let now = Instant::now();
        let mut model = tone_model(now);
        model.gates(true, true, true);
        let mut bytes = vec![255; 98_304];
        assert_eq!(model.fill(&mut bytes, now), (BLOCK_BYTES, true));
        assert_eq!(model.cursor(), BLOCK_BYTES);
        assert!(!model.disabled());
        assert_eq!(model.reply(), 0);
        assert!(bytes[BLOCK_BYTES..].iter().all(|byte| *byte == 255));
    }
    #[test]
    fn b3b_callback_small_capacity_uses_whole_frames_and_silences_tail() {
        for capacity in [8, 9, 3840, 3843, 16_385] {
            let now = Instant::now();
            let mut model = tone_model(now);
            model.gates(true, true, true);
            let mut bytes = vec![255; capacity];
            let expected = capacity.min(BLOCK_BYTES) / STRIDE * STRIDE;
            assert_eq!(model.fill(&mut bytes, now).0, expected);
            assert_eq!(model.cursor(), expected);
            assert!(!model.disabled());
            assert!(
                bytes[expected..capacity.min(BLOCK_BYTES)]
                    .iter()
                    .all(|byte| *byte == 0)
            );
        }
    }
    #[test]
    fn b3b_callback_zero_whole_frame_capacity_fails_and_latches() {
        for capacity in 0..STRIDE {
            let now = Instant::now();
            let mut model = tone_model(now);
            model.gates(true, true, true);
            let mut bytes = vec![255; capacity];
            assert_eq!(model.fill(&mut bytes, now), (0, false));
            assert_eq!(model.reply(), 4);
            assert!(model.disabled());
            assert_eq!(model.cursor(), 0);
            let mut later = [255; 3840];
            assert!(!model.fill(&mut later, now).1);
            assert!(later.iter().all(|byte| *byte == 0));
        }
    }
    #[test]
    fn b3b_callback_final_chunk_is_limited_by_remaining_precomputed_frames() {
        let now = Instant::now();
        let mut model = tone_model(now);
        model.gates(true, true, true);
        let mut bytes = vec![255; 98_304];
        for _ in 0..46 {
            assert_eq!(model.fill(&mut bytes, now).0, BLOCK_BYTES);
        }
        let remaining = FRAMES * STRIDE - model.cursor();
        assert_eq!(remaining, 14_336);
        assert_eq!(model.fill(&mut bytes, now).0, remaining);
        assert_eq!(model.cursor(), FRAMES * STRIDE);
        assert_eq!(model.fill(&mut bytes, now), (0, false));
        assert!(!model.disabled());
    }
    #[test]
    fn b3b_bounded_worker_slot_remains_owned_until_stalled_work_finishes() {
        let leases: Vec<_> = (0..4).map(|_| tone_test::lease().unwrap()).collect();
        assert_eq!(tone_test::slots(), 4);
        assert!(matches!(tone_test::lease(), Err(FixtureError::Busy)));
        let (release, blocked) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let _leases = leases;
            blocked.recv_timeout(Duration::from_secs(2)).unwrap();
        });
        assert_eq!(tone_test::slots(), 4, "no detached-worker capacity reuse");
        release.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(tone_test::slots(), 0);
    }
    #[test]
    fn b3b_test_native_path_default_refuses_before_pipewire_initialization() {
        let pair = UnixStream::pair().unwrap();
        let fd = std::os::fd::OwnedFd::from(pair.0);
        assert_eq!(tone_test::verify_fd(&fd), Err(FixtureError::Refused));
        tone_test::install_guard(Box::new(|_| Err(FixtureError::Refused)));
        assert_eq!(tone_test::phase(), 0);
        let _ = tone_test::diagnostic();
        assert_eq!(tone_test::verify_fd(&fd), Err(FixtureError::Refused));
    }

    #[test]
    fn b3b_autoconnect_override_is_refused_and_cached_without_native_initialization() {
        let mut output = Output::new(Some(PathBuf::from("/nonexistent/inert-runtime")), true);
        assert_eq!(
            output.play(ToneId(1), &speakers()),
            Some(Err(FixtureError::Refused))
        );
        output.forbidden_override = false;
        assert_eq!(
            output.play(ToneId(1), &speakers()),
            Some(Err(FixtureError::Refused))
        );
        assert_eq!(output.state(), OwnToneState::Stopped);
    }
}
