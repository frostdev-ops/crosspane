//! Checked shared-playback conversion and terminal stream lifetime, with no Windows calls.
//! \[E\] Mix descriptors are WAVEFORMATEX/EXTENSIBLE, not a request to retune a device.
//! <https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclient-getmixformat>
//! \[P\] P9g measured stereo float32 only. Other admitted PCM/rates are model-tested, native \[U\].
//! Mono/stereo are supported; unknown channel layouts/precision fail closed.

use crosspane_media::audio::Resampler;
use crosspane_platform::{AudioDeviceError, AudioFormat, IoGate, PlatformError};
use rtrb::Consumer;
use std::{
    fmt,
    mem::ManuallyDrop,
    sync::mpsc::Receiver,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// A borrowed descriptor whose owning destructor must not clear its backing storage.
/// Only an immutable view is exposed; the native owner retains stable backing until final release.
/// This storage is not for descriptors owning separately allocated payloads.
#[allow(dead_code)] // Used by the Windows adapter; its ownership tests also run on other hosts.
#[repr(transparent)]
pub(crate) struct BorrowedDescriptor<T>(ManuallyDrop<T>);

#[allow(dead_code)]
impl<T> BorrowedDescriptor<T> {
    pub(crate) fn new(descriptor: T) -> Self {
        Self(ManuallyDrop::new(descriptor))
    }

    pub(crate) fn get(&self) -> &T {
        &self.0
    }
}

pub const MAX_RENDER_FRAMES: usize = 16_384;
const CHUNK: usize = 256;
const PCM: u16 = 1;
const FLOAT: u16 = 3;
const EXTENSIBLE: u16 = 0xfffe;
const GUID_TAIL: [u8; 12] = [0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71];

fn unsupported() -> PlatformError {
    PlatformError::Unsupported("Windows shared audio format")
}
fn word(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn dword(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoding {
    Float,
    Integer(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MixFormat {
    rate: u32,
    channels: u16,
    encoding: Encoding,
    align: usize,
}
impl MixFormat {
    /// Exact, bounded descriptor; no partially understood extensions or guessed speaker map.
    pub fn parse(bytes: &[u8]) -> Result<Self, PlatformError> {
        let parsed = (|| {
            let tag = word(bytes, 0)?;
            let channels = word(bytes, 2)?;
            let rate = dword(bytes, 4)?;
            let average = dword(bytes, 8)?;
            let align = word(bytes, 12)?;
            let bits = word(bytes, 14)?;
            let extension = word(bytes, 16)?;
            if bytes.len() != 18 + usize::from(extension)
                || !(1..=2).contains(&channels)
                || !(8_000..=384_000).contains(&rate)
            {
                return None;
            }
            let subtype = if tag == EXTENSIBLE {
                let mask = dword(bytes, 20)?;
                if extension != 22
                    || word(bytes, 18)? != bits
                    || !matches!((channels, mask), (1, 0 | 4) | (2, 0 | 3))
                    || bytes.get(28..40)? != GUID_TAIL
                {
                    return None;
                }
                u16::try_from(dword(bytes, 24)?).ok()?
            } else {
                if extension != 0 {
                    return None;
                }
                tag
            };
            let encoding = match (subtype, bits) {
                (FLOAT, 32) => Encoding::Float,
                (PCM, 16 | 24 | 32) => Encoding::Integer(bits),
                _ => return None,
            };
            if align != channels.checked_mul(bits / 8)?
                || average != rate.checked_mul(u32::from(align))?
            {
                return None;
            }
            Some(Self {
                rate,
                channels,
                encoding,
                align: usize::from(align),
            })
        })();
        parsed.ok_or_else(unsupported)
    }
    pub fn rate(&self) -> u32 {
        self.rate
    }
    pub fn channels(&self) -> u16 {
        self.channels
    }
    pub fn bytes_for(&self, frames: usize) -> Result<usize, PlatformError> {
        if frames > MAX_RENDER_FRAMES {
            return Err(unsupported());
        }
        frames.checked_mul(self.align).ok_or_else(unsupported)
    }
    fn encode(&self, value: f32, bytes: &mut [u8]) {
        let value = if value.is_finite() { value } else { 0.0 };
        match self.encoding {
            Encoding::Float => bytes.copy_from_slice(&value.to_le_bytes()),
            Encoding::Integer(bits) => {
                let scale = (1i64 << (bits - 1)) as f64;
                let n = (f64::from(value).clamp(-1.0, 1.0) * scale)
                    .round()
                    .clamp(-scale, scale - 1.0) as i32;
                bytes.copy_from_slice(&n.to_le_bytes()[..usize::from(bits / 8)]);
            }
        }
    }
}

/// Tables and both scratches are allocated at open; render never allocates or logs samples.
pub struct Converter {
    mix: MixFormat,
    channels: usize,
    resampler: Resampler,
    input: Vec<f32>,
    output: Vec<f32>,
}
impl fmt::Debug for Converter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AudioConverter(..)")
    }
}
impl Converter {
    pub fn new(source: AudioFormat, mix: MixFormat) -> Result<Self, PlatformError> {
        if !source.is_valid() {
            return Err(unsupported());
        }
        let resampler =
            Resampler::new(source.rate, mix.rate, source.channels).map_err(|_| unsupported())?;
        let channels = usize::from(source.channels);
        // One extra input frame covers all phase positions, not only the initial one.
        let frames = (u64::from(source.rate) * CHUNK as u64).div_ceil(u64::from(mix.rate)) + 1;
        let input = vec![0.0; frames as usize * channels];
        Ok(Self {
            mix,
            channels,
            resampler,
            input,
            output: vec![0.0; CHUNK * channels],
        })
    }
    pub fn render(
        &mut self,
        pcm: &mut Consumer<f32>,
        frames: usize,
        bytes: &mut [u8],
    ) -> Result<(), PlatformError> {
        if bytes.len() != self.mix.bytes_for(frames)? {
            return Err(unsupported());
        }
        for chunk in bytes.chunks_mut(CHUNK * self.mix.align) {
            let frames = chunk.len() / self.mix.align;
            let needed = self.resampler.input_frames_for(frames) * self.channels;
            let input = self.input.get_mut(..needed).ok_or_else(unsupported)?;
            input.fill(0.0);
            // Leave an unmatched half frame in the ring; never rotate L/R after an underrun.
            let available = (pcm.slots() / self.channels * self.channels).min(needed);
            for value in &mut input[..available] {
                let sample = pcm.pop().map_err(|_| unsupported())?;
                *value = if sample.is_finite() { sample } else { 0.0 };
            }
            let output = &mut self.output[..frames * self.channels];
            let processed = self.resampler.process(input, output);
            if processed.produced != frames || processed.consumed * self.channels != needed {
                return Err(unsupported());
            }
            let width = self.mix.align / usize::from(self.mix.channels);
            for (frame, target) in output
                .chunks_exact(self.channels)
                .zip(chunk.chunks_exact_mut(self.mix.align))
            {
                for (channel, sample) in target.chunks_exact_mut(width).enumerate() {
                    let value = if self.mix.channels == 1 && self.channels == 2 {
                        // Half first, so two finite full-scale f32 values cannot overflow.
                        frame[0] * 0.5 + frame[1] * 0.5
                    } else {
                        frame[channel.min(self.channels - 1)]
                    };
                    self.mix.encode(value, sample);
                }
            }
        }
        Ok(())
    }
}

const ACTIVE: u8 = 0;
const CANCELLED: u8 = 1;
const LOCKED: u8 = 2;
const FAILED: u8 = 3;

/// Only terminal transitions. Gate ABA, host loss, cancellation and device loss never re-arm.
#[derive(Debug)]
pub struct StreamControl {
    gate: Arc<IoGate>,
    epoch: u64,
    state: AtomicU8,
    retired: AtomicBool,
}
impl StreamControl {
    pub fn new(gate: Arc<IoGate>) -> Self {
        let epoch = gate.epoch();
        Self {
            gate,
            epoch,
            state: AtomicU8::new(ACTIVE),
            retired: AtomicBool::new(false),
        }
    }
    fn latch(&self, state: u8) {
        let _ = self
            .state
            .compare_exchange(ACTIVE, state, Ordering::AcqRel, Ordering::Acquire);
    }
    pub fn permits(&self) -> bool {
        if !self.gate.is_open() || self.gate.epoch() != self.epoch {
            self.latch(LOCKED);
        }
        self.state.load(Ordering::Acquire) == ACTIVE
    }
    pub fn cancel(&self) {
        self.latch(CANCELLED);
    }
    pub fn fail(&self) {
        self.latch(FAILED);
    }
    pub fn reason(&self) -> Option<AudioDeviceError> {
        match self.state.load(Ordering::Acquire) {
            LOCKED => Some(AudioDeviceError::Locked),
            FAILED => Some(AudioDeviceError::Failed),
            _ => None,
        }
    }
    /// Open-time refusal preserves device/cancellation evidence; it never invents a closed gate.
    pub fn open_refusal(&self) -> PlatformError {
        match self.state.load(Ordering::Acquire) {
            LOCKED => PlatformError::Locked,
            CANCELLED => PlatformError::Timeout,
            _ => PlatformError::Backend("Windows playback device unavailable".into()),
        }
    }
    pub fn retire(&self) {
        self.retired.store(true, Ordering::Release);
    }
    pub fn is_retired(&self) -> bool {
        self.retired.load(Ordering::Acquire)
    }
    /// Cancellation precedes the bounded wait; a late owner still retains the disabled state.
    pub fn wait_retired(&self, deadline: Instant) {
        while !self.is_retired() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
    }
}

/// One shared open deadline, including receipt. Success after cancellation is never published.
pub fn wait_open(
    reply: &Receiver<Result<(), PlatformError>>,
    deadline: Instant,
    control: &StreamControl,
) -> Result<(), PlatformError> {
    let answer = reply.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    let result = match answer {
        Ok(Ok(())) if Instant::now() >= deadline => Err(PlatformError::Timeout),
        Ok(Ok(())) if control.permits() => Ok(()),
        Ok(Ok(())) => Err(control.open_refusal()),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(PlatformError::Timeout),
    };
    if result.is_err() {
        control.cancel();
    }
    result
}

/// Bounded root count and one 10 ms frame of the frozen 48 kHz stereo Speaker format.
pub const MAX_PEER_SOURCES: usize = 32;
pub const SPEAKER_FRAME_SAMPLES: usize = 960;

/// Per-revision PCM eligibility, distinct from permanent owner retirement. Native control uses
/// disable BEFORE a possibly stalled Stop/release. Only a deliberate changed set replaces the
/// revision; callbacks cannot re-arm it. This grants no PID, projection or received-grant authority.
pub struct SourceLifecycle {
    gate: Arc<IoGate>,
    epoch: AtomicU64,
    enabled: AtomicBool,
    cancelled: AtomicBool,
}
impl fmt::Debug for SourceLifecycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SourceLifecycle(..)")
    }
}
impl SourceLifecycle {
    pub fn new(gate: Arc<IoGate>) -> Self {
        Self {
            epoch: AtomicU64::new(gate.epoch()),
            gate,
            enabled: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        }
    }
    pub fn replace_revision(&self, observed_epoch: u64, enabled: bool) {
        self.epoch.store(observed_epoch, Ordering::Release);
        self.enabled
            .store(enabled && !self.is_cancelled(), Ordering::Release);
    }
    pub fn disable(&self) {
        self.enabled.store(false, Ordering::Release);
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.disable();
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    pub fn permits(&self) -> bool {
        !self.is_cancelled()
            && self.enabled.load(Ordering::Acquire)
            && self.gate.is_open()
            && self.gate.epoch() == self.epoch.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
pub struct SourceDelta {
    pub revision: u64,
    pub start: Vec<u32>,
    pub stop: Vec<u32>,
    pub changed: bool,
    pub active: Option<bool>,
    pub error: Option<AudioDeviceError>,
}
#[derive(Debug)]
pub struct SourceFailure {
    pub active: Option<bool>,
    pub error: AudioDeviceError,
}
#[derive(Debug)]
pub struct SourceSet {
    supported: bool,
    unavailable_reported: bool,
    desired: Vec<u32>,
    running: Vec<u32>,
    revision: u64,
    terminal: bool,
}
impl SourceSet {
    /// The revision correlates results only; PID reuse is explicitly unverified \[U\].
    /// A Failed revision is peer-terminal because the frozen engine's error ends the peer stream.
    pub fn new(supported: bool) -> Self {
        Self {
            supported,
            unavailable_reported: false,
            desired: Vec::new(),
            running: Vec::with_capacity(MAX_PEER_SOURCES),
            revision: 0,
            terminal: false,
        }
    }
    pub fn replace(&mut self, pids: &[u32], permitted: bool) -> Result<SourceDelta, PlatformError> {
        if pids.len() > MAX_PEER_SOURCES || pids.contains(&0) {
            return Err(PlatformError::Unsupported(
                "Windows source audio root bound",
            ));
        }
        if !pids.is_empty() && !permitted {
            return Err(PlatformError::Locked);
        }
        let mut desired = pids.to_vec();
        desired.sort_unstable();
        desired.dedup();
        let mut delta = SourceDelta {
            revision: self.revision,
            start: Vec::new(),
            stop: Vec::new(),
            changed: false,
            active: None,
            error: None,
        };
        if desired == self.desired {
            return Ok(delta);
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or(PlatformError::Unsupported(
                "Windows source audio revisions exhausted",
            ))?;
        let was_active = !self.running.is_empty();
        delta.stop = self
            .running
            .iter()
            .copied()
            .filter(|pid| !desired.contains(pid))
            .collect();
        self.running.retain(|pid| desired.contains(pid));
        if was_active && self.running.is_empty() {
            delta.active = Some(false);
        }
        if self.supported {
            // Pending roots from the old revision must get a new admission, not a stale result.
            delta.start = desired
                .iter()
                .copied()
                .filter(|pid| !self.running.contains(pid))
                .collect();
        } else if !desired.is_empty() && !self.unavailable_reported {
            self.unavailable_reported = true;
            delta.error = Some(AudioDeviceError::Unavailable);
        }
        self.desired = desired;
        self.revision = revision;
        self.terminal = !self.supported;
        delta.revision = revision;
        delta.changed = true;
        Ok(delta)
    }
    pub fn started(&mut self, revision: u64, pid: u32) -> Option<bool> {
        if self.terminal
            || revision != self.revision
            || !self.desired.contains(&pid)
            || self.running.contains(&pid)
        {
            return None;
        }
        let was_empty = self.running.is_empty();
        self.running.push(pid);
        self.running.sort_unstable();
        was_empty.then_some(true)
    }
    /// The caller fences ALL peer PCM before teardown, publishing inactive before the one error.
    pub fn failed(&mut self, revision: u64, pid: u32) -> Option<SourceFailure> {
        if self.terminal || revision != self.revision || !self.desired.contains(&pid) {
            return None;
        }
        self.terminal = true;
        let was_active = !self.running.is_empty();
        self.running.clear();
        Some(SourceFailure {
            active: was_active.then_some(false),
            error: AudioDeviceError::Failed,
        })
    }
    pub fn gate_closed(&mut self) -> Option<bool> {
        self.terminal = true;
        let was_active = !self.running.is_empty();
        self.running.clear();
        was_active.then_some(false)
    }
    pub fn running(&self) -> &[u32] {
        &self.running
    }
}
/// No allocation in begin/add/finish; preserve complete stereo frames through underrun.
/// Wider accumulation prevents finite full-scale source values overflowing before final clamp.
pub struct SpeakerMixer {
    sum: [f64; SPEAKER_FRAME_SAMPLES],
}
impl fmt::Debug for SpeakerMixer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SpeakerMixer(..)")
    }
}
impl Default for SpeakerMixer {
    fn default() -> Self {
        Self::new()
    }
}
impl SpeakerMixer {
    pub fn new() -> Self {
        Self {
            sum: [0.0; SPEAKER_FRAME_SAMPLES],
        }
    }
    pub fn begin(&mut self) {
        self.sum.fill(0.0);
    }
    pub fn add(&mut self, pcm: &mut Consumer<f32>) {
        let samples = (pcm.slots() / 2 * 2).min(SPEAKER_FRAME_SAMPLES);
        for sum in &mut self.sum[..samples] {
            let Ok(value) = pcm.pop() else { break };
            if value.is_finite() {
                *sum += f64::from(value);
            }
        }
    }
    pub fn finish(&self, output: &mut [f32; SPEAKER_FRAME_SAMPLES]) {
        for (sample, sum) in output.iter_mut().zip(self.sum) {
            *sample = if sum.is_finite() {
                sum.clamp(-1.0, 1.0) as f32
            } else {
                0.0
            };
        }
    }
}

/// Whole 10 ms frames a capture may keep queued after a mixing pass; anything older is discarded.
pub const MAX_CAPTURE_BACKLOG: usize = 2; // frames
/// A capture that has delivered no data for this long is idle: the mixer neither waits for it
/// nor adds it, so a silent process cannot stall the others.
pub const CAPTURE_IDLE: Duration = Duration::from_millis(30);

/// Whole 10 ms frames to mix now, clocked by captured data (the audio engine), never by
/// `Instant`: the minimum over captures not idle (no data for 30 ms), or 0. `queued` is in
/// interleaved samples.
///
/// `queued[i]` pairs with `idle[i]`; extra entries in the longer slice are ignored.
// Found while reviewing audio/video sync raised by Thiago Goncalves (2026-10).
pub fn frames_to_mix(queued: &[usize], idle: &[bool]) -> usize {
    queued
        .iter()
        .zip(idle)
        .filter(|(_, idle)| !**idle)
        .map(|(queued, _)| queued / SPEAKER_FRAME_SAMPLES)
        .min()
        .unwrap_or(0)
}

/// Samples to discard from the front of a capture holding `queued` after mixing, keeping at
/// most MAX_CAPTURE_BACKLOG frames.
///
/// The result is even, so discarding never rotates left and right; an unmatched half frame at
/// the back stays queued.
pub fn capture_excess(queued: usize) -> usize {
    queued.saturating_sub(MAX_CAPTURE_BACKLOG * SPEAKER_FRAME_SAMPLES) / 2 * 2
}

/// A reopen attempt may start at most this many times per [`REOPEN_WINDOW`] for one stream.
pub const REOPEN_MAX_ATTEMPTS: usize = 5;
/// The sliding window over which [`REOPEN_MAX_ATTEMPTS`] is counted.
pub const REOPEN_WINDOW: Duration = Duration::from_secs(60);
const REOPEN_BACKOFF_BASE: Duration = Duration::from_millis(50);
const REOPEN_BACKOFF_CAP: Duration = Duration::from_millis(1_600);

/// What the owner of a render stream does about the next reopen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReopenStep {
    /// Wait this long (it also coalesces a burst of notifications), then attempt the reopen.
    Attempt { after: Duration },
    /// The per-minute budget is spent: end the stream exactly as a device loss ended it before.
    GiveUp,
}

/// Bounded reopen policy for one stream: at most [`REOPEN_MAX_ATTEMPTS`] attempts in any
/// [`REOPEN_WINDOW`], with an exponential backoff (50 ms doubling, capped) between consecutive
/// failed attempts. Every attempt, successful or not, counts against the budget, so a flapping
/// device cannot make a stream reopen forever. Pure: callers pass the clock.
#[derive(Debug)]
#[allow(dead_code)] // Used by the Windows adapter; its policy tests also run on other hosts.
pub struct ReopenLimiter {
    starts: [Option<Instant>; REOPEN_MAX_ATTEMPTS],
    next_slot: usize,
    failures: u32,
}
impl Default for ReopenLimiter {
    fn default() -> Self {
        Self::new()
    }
}
#[allow(dead_code)]
impl ReopenLimiter {
    pub fn new() -> Self {
        Self {
            starts: [None; REOPEN_MAX_ATTEMPTS],
            next_slot: 0,
            failures: 0,
        }
    }
    /// Delay before the attempt that follows `failures` consecutive failures.
    pub fn backoff(failures: u32) -> Duration {
        REOPEN_BACKOFF_BASE
            .saturating_mul(1u32 << failures.min(10))
            .min(REOPEN_BACKOFF_CAP)
    }
    /// Plans the next attempt at `now`. On [`ReopenStep::Attempt`] the attempt is recorded as
    /// starting at `now + after`, the instant at which the budget is judged.
    pub fn next(&mut self, now: Instant) -> ReopenStep {
        let after = Self::backoff(self.failures);
        let at = now + after;
        let recent = self
            .starts
            .iter()
            .flatten()
            .filter(|start| at.saturating_duration_since(**start) < REOPEN_WINDOW)
            .count();
        if recent >= REOPEN_MAX_ATTEMPTS {
            return ReopenStep::GiveUp;
        }
        self.starts[self.next_slot] = Some(at);
        self.next_slot = (self.next_slot + 1) % REOPEN_MAX_ATTEMPTS;
        ReopenStep::Attempt { after }
    }
    /// The last planned attempt opened a client: the next change starts again without backoff.
    pub fn succeeded(&mut self) {
        self.failures = 0;
    }
    /// The last planned attempt failed: lengthen the next backoff.
    pub fn failed(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }
}

/// One MMDevice notification, reduced to the facts the relevance decision needs. Endpoint IDs are
/// UTF-16 without their terminator; contents are compared, never parsed or logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum EndpointEvent<'a> {
    /// `OnDefaultDeviceChanged`; `id` is `None` when no default endpoint remains.
    DefaultChanged {
        render: bool,
        console: bool,
        id: Option<&'a [u16]>,
    },
    /// `OnDeviceStateChanged`; `active` is whether the new state is `DEVICE_STATE_ACTIVE`.
    State { id: &'a [u16], active: bool },
    /// `OnDeviceRemoved`.
    Removed { id: &'a [u16] },
    /// `OnPropertyValueChanged`: conservatively invalidates the negotiated mix of that endpoint.
    Property { id: &'a [u16] },
}

/// True when `event` means the stream's endpoint `current` (the console render default captured
/// at open) is no longer the right one: another endpoint became the default, or `current` left the
/// active state, was removed, or changed properties. Capture/communications changes are not ours.
#[allow(dead_code)]
pub fn endpoint_event_relevant(current: &[u16], event: EndpointEvent<'_>) -> bool {
    match event {
        EndpointEvent::DefaultChanged {
            render,
            console,
            id,
        } => render && console && id != Some(current),
        EndpointEvent::State { id, active } => id == current && !active,
        EndpointEvent::Removed { id } | EndpointEvent::Property { id } => id == current,
    }
}

/// Latch from the notification thread to the stream owner. The callback only raises it (and
/// signals the owner's event); the owner alone clears it and calls into WASAPI.
#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct ChangeSignal(AtomicBool);
#[allow(dead_code)]
impl ChangeSignal {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn raise(&self) {
        self.0.store(true, Ordering::Release);
    }
    /// Consumes a raised change; true at most once per `raise` burst.
    pub fn take(&self) -> bool {
        self.0.swap(false, Ordering::AcqRel)
    }
}

#[cfg(test)]
mod reopen_tests {
    use super::*;

    fn ids(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn backoff_doubles_from_fifty_milliseconds_and_caps() {
        let steps: Vec<_> = (0..8).map(ReopenLimiter::backoff).collect();
        assert_eq!(
            steps,
            [50, 100, 200, 400, 800, 1_600, 1_600, 1_600].map(Duration::from_millis)
        );
        assert_eq!(ReopenLimiter::backoff(u32::MAX), REOPEN_BACKOFF_CAP);
    }

    #[test]
    fn attempts_are_capped_per_minute_and_recover_after_the_window() {
        let base = Instant::now();
        let mut limiter = ReopenLimiter::new();
        for _ in 0..REOPEN_MAX_ATTEMPTS {
            assert!(matches!(limiter.next(base), ReopenStep::Attempt { .. }));
            limiter.succeeded();
        }
        assert_eq!(limiter.next(base), ReopenStep::GiveUp);
        assert_eq!(
            limiter.next(base + Duration::from_secs(59)),
            ReopenStep::GiveUp
        );
        // Every recorded attempt has left the window, so the budget is available again.
        assert!(matches!(
            limiter.next(base + Duration::from_secs(61)),
            ReopenStep::Attempt { .. }
        ));
    }

    #[test]
    fn failures_back_off_and_success_resets_them() {
        let now = Instant::now();
        let mut limiter = ReopenLimiter::new();
        let mut waits = Vec::new();
        for _ in 0..3 {
            let ReopenStep::Attempt { after } = limiter.next(now) else {
                panic!("budget unexpectedly spent");
            };
            waits.push(after);
            limiter.failed();
        }
        assert_eq!(waits, [50, 100, 200].map(Duration::from_millis));
        limiter.succeeded();
        assert_eq!(
            limiter.next(now),
            ReopenStep::Attempt {
                after: Duration::from_millis(50)
            }
        );
    }

    #[test]
    fn five_consecutive_failures_exhaust_the_episode_then_give_up() {
        let now = Instant::now();
        let mut limiter = ReopenLimiter::new();
        for _ in 0..REOPEN_MAX_ATTEMPTS {
            assert!(matches!(limiter.next(now), ReopenStep::Attempt { .. }));
            limiter.failed();
        }
        assert_eq!(limiter.next(now), ReopenStep::GiveUp);
    }

    #[test]
    fn default_change_is_relevant_only_for_console_render_to_another_endpoint() {
        let (speakers, headset) = (ids("{0.0.0.00000000}.speakers"), ids("{0.0.0.00000000}.hs"));
        let default = |render, console, id| EndpointEvent::DefaultChanged {
            render,
            console,
            id,
        };
        assert!(endpoint_event_relevant(
            &speakers,
            default(true, true, Some(&headset))
        ));
        // No default endpoint remains: the stream cannot stay where it is.
        assert!(endpoint_event_relevant(
            &speakers,
            default(true, true, None)
        ));
        // The notification confirms the endpoint already in use, or is for another flow or role.
        assert!(!endpoint_event_relevant(
            &speakers,
            default(true, true, Some(&speakers))
        ));
        assert!(!endpoint_event_relevant(
            &speakers,
            default(false, true, Some(&headset))
        ));
        assert!(!endpoint_event_relevant(
            &speakers,
            default(true, false, Some(&headset))
        ));
    }

    #[test]
    fn state_removal_and_property_changes_matter_only_for_the_active_endpoint() {
        let (speakers, headset) = (ids("speakers"), ids("headset"));
        for other in [
            EndpointEvent::State {
                id: &headset,
                active: false,
            },
            EndpointEvent::Removed { id: &headset },
            EndpointEvent::Property { id: &headset },
        ] {
            assert!(!endpoint_event_relevant(&speakers, other));
        }
        assert!(endpoint_event_relevant(
            &speakers,
            EndpointEvent::State {
                id: &speakers,
                active: false
            }
        ));
        assert!(!endpoint_event_relevant(
            &speakers,
            EndpointEvent::State {
                id: &speakers,
                active: true
            }
        ));
        assert!(endpoint_event_relevant(
            &speakers,
            EndpointEvent::Removed { id: &speakers }
        ));
        assert!(endpoint_event_relevant(
            &speakers,
            EndpointEvent::Property { id: &speakers }
        ));
        // A prefix or extension of our ID is a different endpoint.
        let longer = ids("speakers2");
        assert!(!endpoint_event_relevant(
            &speakers,
            EndpointEvent::Removed { id: &longer }
        ));
        assert!(!endpoint_event_relevant(
            &speakers,
            EndpointEvent::Removed { id: &speakers[..4] }
        ));
    }

    #[test]
    fn change_signal_coalesces_a_burst_into_one_take() {
        let signal = ChangeSignal::new();
        assert!(!signal.take());
        signal.raise();
        signal.raise();
        assert!(signal.take());
        assert!(!signal.take());
    }
}

#[cfg(test)]
mod borrowed_activation_tests {
    use super::BorrowedDescriptor;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct DescriptorSpy<'a> {
        clears: &'a AtomicUsize,
        value: usize,
    }

    impl Drop for DescriptorSpy<'_> {
        fn drop(&mut self) {
            // A safe scalar spy stands in for a descriptor clearing borrowed storage.
            self.clears.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct BackingSpy<'a>(&'a AtomicUsize);

    impl Drop for BackingSpy<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct Owner<'a> {
        descriptor: BorrowedDescriptor<DescriptorSpy<'a>>,
        _backing: BackingSpy<'a>,
    }

    fn owner<'a>(clears: &'a AtomicUsize, drops: &'a AtomicUsize) -> Arc<Owner<'a>> {
        Arc::new(Owner {
            descriptor: BorrowedDescriptor::new(DescriptorSpy { clears, value: 17 }),
            _backing: BackingSpy(drops),
        })
    }

    #[test]
    fn borrowed_activation_descriptor_never_clears_inline_backing() {
        let clears = AtomicUsize::new(0);
        let drops = AtomicUsize::new(0);
        let allocation = owner(&clears, &drops);
        assert_eq!(allocation.descriptor.get().value, 17);
        drop(allocation);
        assert_eq!(clears.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn borrowed_activation_storage_survives_retained_reference() {
        let clears = AtomicUsize::new(0);
        let drops = AtomicUsize::new(0);
        let local = owner(&clears, &drops);
        let retained = local.clone();
        drop(local);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(clears.load(Ordering::SeqCst), 0);
        assert_eq!(retained.descriptor.get().value, 17);
        drop(retained);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(clears.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn borrowed_activation_error_exits_do_not_clear_borrowed_parameters() {
        // Different retained-reference counts model early error, cancelled wait and late completion.
        // These are storage-lifetime scopes, not an assertion about the operating system's COM calls.
        for retained_count in 0..=2 {
            let clears = AtomicUsize::new(0);
            let drops = AtomicUsize::new(0);
            let local = owner(&clears, &drops);
            let retained: Vec<_> = (0..retained_count).map(|_| local.clone()).collect();
            drop(local);
            assert_eq!(
                drops.load(Ordering::SeqCst),
                usize::from(retained_count == 0)
            );
            drop(retained);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(clears.load(Ordering::SeqCst), 0);
        }
    }
}
