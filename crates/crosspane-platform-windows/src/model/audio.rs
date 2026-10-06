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
