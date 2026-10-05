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
    sync::mpsc::Receiver,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

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
