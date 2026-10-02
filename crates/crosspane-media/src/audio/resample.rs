//! Streaming fixed-ratio band-limited resampler for interleaved `f32` PCM.
//!
//! # Method
//!
//! Polyphase windowed sinc with the exact rational phases of the reduced ratio `to/from =
//! L/M`. The filter has [`TAPS`] taps per output sample per channel and the output sample `n`
//! sits at the input position `n * M / L`, so phase `(n * M) mod L` selects one of `L` coefficient
//! rows and no phase interpolation (and no drift) is involved. All coefficient rows are computed
//! by [`Resampler::new`]; [`Resampler::process`] and [`Resampler::reset`] never allocate.
//!
//! # Design
//!
//! The kernel is `h(d) = 2 fc sinc(2 fc d) w(d / K)` with `K = TAPS / 2` and `d` in input frames.
//! The window `w` is a Kaiser window with its edge pedestal removed,
//! `w(u) = (I0(beta sqrt(1 - u^2)) - 1) / (I0(beta) - 1)`. The window therefore reaches exactly
//! zero at `|d| = K`. Against a plain Kaiser window with the same `beta` this lowers the stop band
//! around one cycle per input frame (where the images of low-frequency tones land, which limits
//! the accuracy of a pure tone) by about 13 dB, and costs about 2.5 dB of the best attainable
//! attenuation next to the transition. Every phase row is scaled to unity DC gain.
//!
//! With `fN = 0.5 * min(1, to/from)` the lower Nyquist frequency (in cycles per input frame):
//!
//! * the stop-band edge is the lower Nyquist frequency `fN`;
//! * the half-width of the transition band is `h = min(0.034, 0.25 fN)` input cycles per frame
//!   (3.3 kHz wide at 48 kHz, so for 48 kHz to 44.1 kHz it spans 18.8 kHz to 22.05 kHz);
//! * the cutoff is `fc = fN - h`, always below `fN`, so it never exceeds `0.5 * min(from, to)`;
//! * `beta = 7 * h / 0.034`.
//!
//! The `0.25 fN` cap only matters for decimation by more than about 3.6:1, where 64 taps cannot
//! hold the full transition width; the pass band then narrows (never past `fN / 2`) instead of the
//! cutoff crossing the Nyquist frequency.
//!
//! For 48 kHz to 44.1 kHz (147 phases) this gives `fc` = 20.42 kHz, a pass band within 0.06 dB up
//! to 19 kHz, and 67.9 dB of attenuation or more from 22.05 kHz up to 24 kHz (the filter's own
//! response). Input tones swept from 22.06 kHz to 23.95 kHz in 25 Hz steps come out 67.7 dB
//! (weakest, at 22.65 kHz) or more down; the response has notches (23.0 kHz is one, at about
//! -87 dB) between its peaks. 64 taps cannot do much better with this family at the same pass
//! band: with 0.1 dB at 19 kHz no `beta` and `fc` keep the whole band from 22.05 kHz up to 24 kHz
//! below -72 dB (plain Kaiser) or -70 dB (this window).
//!
//! That stop band is enough because the audio that reaches this resampler is decoded Opus, and
//! Opus fullband carries at most 20 kHz: there is essentially nothing between 22.05 kHz and
//! 24 kHz to alias into the output. For 48 kHz to 96 kHz or 88.2 kHz the same shape sits at `fc`
//! = 22.37 kHz: flat to within 0.002 dB up to 19 kHz, -2.9 dB at 22 kHz and at least 67 dB down
//! from 24 kHz.
//!
//! # Streaming contract
//!
//! The output is a delayed copy of the input: output frame `n` is the band-limited signal at
//! input time `n * M / L - K` frames, so the group delay is `K` input frames, which
//! [`Resampler::latency_frames`] reports in output frames. Before the first input the history is
//! silence.
//!
//! `process` consumes exactly the input frames the produced output frames need, no more. Input that
//! cannot yet contribute to another output frame stays unconsumed: the caller offers it again with
//! the next call, or sizes each call with [`Resampler::input_frames_for`], which makes
//! `produced == out_frames` exact. Because each output frame depends only on a fixed window of
//! input frames and a fixed phase row, the concatenated output is bit-identical however the input
//! and the output are split into calls.

use super::AudioError;
use std::fmt;

/// Filter taps per output sample per channel.
const TAPS: usize = 64;
/// Half-length of the kernel in input frames.
const HALF: usize = TAPS / 2;
/// Frames of new input kept before the sliding history is moved back to the start of its buffer.
const BLOCK: usize = 256;
/// Capacity of the sliding history, per channel, in frames.
const CAPACITY: usize = TAPS + BLOCK;
/// Accepted sample rates in Hz.
const RATES: std::ops::RangeInclusive<u32> = 8_000..=384_000;
/// Most polyphase phases (the denominator of the reduced ratio) a resampler accepts.
const MAX_PHASES: u32 = 1024;
/// Widest transition half-width, in cycles per input frame.
const HALF_TRANSITION: f64 = 0.034;
/// Kaiser `beta` at [`HALF_TRANSITION`]; it scales linearly with a narrower transition.
const REFERENCE_BETA: f64 = 7.0;

/// Frames (not samples) moved by one [`Resampler::process`] call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Processed {
    /// Input frames consumed.
    pub consumed: usize,
    /// Output frames produced.
    pub produced: usize,
}

/// Streaming fixed-ratio band-limited resampler for interleaved f32 PCM.
/// All tables and history are allocated by `new`; `process` and `reset` never allocate.
pub struct Resampler {
    from: u32,
    to: u32,
    channels: u16,
    /// Denominator `L` of the reduced ratio `to/from`: the number of polyphase phases.
    phases: u32,
    /// Numerator `M` of the reduced ratio `from/to`: the phase step per output frame.
    step: u32,
    /// `phases * TAPS` coefficients, one contiguous row per phase.
    coefficients: Box<[f32]>,
    /// Planar sliding history, `CAPACITY` frames per channel.
    history: Box<[f32]>,
    /// Valid frames in each channel's history; the newest `TAPS` of them are the filter window.
    filled: usize,
    /// Newest input index the next output needs, minus the input frames consumed so far. The next
    /// output can be produced when this is `-1`; it is never below `-1`.
    lead: i64,
    /// Phase row of the next output, in `0..phases`.
    phase: u32,
    latency: usize,
}

impl fmt::Debug for Resampler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resampler")
            .field("from", &self.from)
            .field("to", &self.to)
            .field("channels", &self.channels)
            .field("phases", &self.phases)
            .finish_non_exhaustive()
    }
}

impl Resampler {
    /// `from` and `to` in Hz, each in 8_000..=384_000; `channels` 1 or 2. Errors with
    /// `AudioError::InvalidFormat` outside those ranges, or when the reduced ratio needs more than
    /// 1024 polyphase phases (every standard rate pair from 48 kHz is well inside that).
    pub fn new(from: u32, to: u32, channels: u16) -> Result<Self, AudioError> {
        if !RATES.contains(&from) || !RATES.contains(&to) || !(1..=2).contains(&channels) {
            return Err(AudioError::InvalidFormat);
        }
        let divisor = gcd(from, to);
        let (phases, step) = (to / divisor, from / divisor);
        if phases > MAX_PHASES {
            return Err(AudioError::InvalidFormat);
        }
        let identity = from == to;
        let coefficients = if identity {
            Box::default()
        } else {
            design(from, to, phases)
        };
        let latency = if identity {
            0
        } else {
            // K input frames expressed in output frames, rounded to the nearest frame.
            let numerator = (HALF as u64) * u64::from(to);
            usize::try_from((2 * numerator + u64::from(from)) / (2 * u64::from(from)))
                .unwrap_or(usize::MAX)
        };
        let history = if identity {
            Box::default()
        } else {
            vec![0.0; usize::from(channels) * CAPACITY].into_boxed_slice()
        };
        Ok(Self {
            from,
            to,
            channels,
            phases,
            step,
            coefficients,
            history,
            filled: TAPS - 1,
            lead: 0,
            phase: 0,
            latency,
        })
    }

    /// Input sample rate in Hz.
    pub fn from_rate(&self) -> u32 {
        self.from
    }

    /// Output sample rate in Hz.
    pub fn to_rate(&self) -> u32 {
        self.to
    }

    /// Channels per frame (1 or 2).
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// True when `from == to`: `process` is then an exact copy with zero latency.
    pub fn is_identity(&self) -> bool {
        self.from == self.to
    }

    /// Exact number of input frames the next `process` call needs to produce `out_frames`
    /// output frames, given the current phase and history.
    pub fn input_frames_for(&self, out_frames: usize) -> usize {
        if self.is_identity() || out_frames == 0 {
            return out_frames;
        }
        // The last of the `out_frames` outputs needs the input up to index `lead + 1 + carries`,
        // where `carries` is how far the phase accumulator wraps in `out_frames - 1` outputs.
        let advance = u128::from(self.phase) + (out_frames as u128 - 1) * u128::from(self.step);
        let carries = advance / u128::from(self.phases);
        let already = (self.lead + 1).max(0) as u128;
        usize::try_from(already + carries).unwrap_or(usize::MAX)
    }

    /// Produce as many output frames as possible, up to `output.len() / channels`, consuming
    /// input frames only as needed (never more than `input_frames_for(produced)`). `input` and
    /// `output` are interleaved and hold whole frames (panics otherwise; that is a caller bug).
    /// The concatenated output is identical however the input and output are split across calls.
    ///
    /// Input that cannot yet contribute to another output frame is not consumed: offer it again
    /// with the next call. Every call consumes exactly `input_frames_for(produced)` frames,
    /// measured before the call.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> Processed {
        let channels = usize::from(self.channels);
        assert!(
            input.len().is_multiple_of(channels) && output.len().is_multiple_of(channels),
            "resampler buffers must hold whole frames"
        );
        let (in_frames, out_frames) = (input.len() / channels, output.len() / channels);
        if self.is_identity() {
            let frames = in_frames.min(out_frames);
            output[..frames * channels].copy_from_slice(&input[..frames * channels]);
            return Processed {
                consumed: frames,
                produced: frames,
            };
        }
        let (mut consumed, mut produced) = (0, 0);
        while produced < out_frames {
            let needed = (self.lead + 1) as usize;
            if needed > 0 {
                if in_frames - consumed < needed {
                    break;
                }
                for frame in input[consumed * channels..(consumed + needed) * channels]
                    .chunks_exact(channels)
                {
                    self.push(frame);
                }
                consumed += needed;
                self.lead = -1;
            }
            self.emit(&mut output[produced * channels..(produced + 1) * channels]);
            produced += 1;
            self.phase += self.step;
            self.lead += i64::from(self.phase / self.phases);
            self.phase %= self.phases;
        }
        Processed { consumed, produced }
    }

    /// Forget history and phase, as for a fresh stream.
    pub fn reset(&mut self) {
        self.history.fill(0.0);
        self.filled = TAPS - 1;
        self.lead = 0;
        self.phase = 0;
    }

    /// Group delay in output frames (0 for identity).
    pub fn latency_frames(&self) -> usize {
        self.latency
    }

    /// Append one input frame to every channel's history.
    fn push(&mut self, frame: &[f32]) {
        if self.filled == CAPACITY {
            for channel in 0..frame.len() {
                let base = channel * CAPACITY;
                self.history
                    .copy_within(base + CAPACITY - (TAPS - 1)..base + CAPACITY, base);
            }
            self.filled = TAPS - 1;
        }
        for (channel, sample) in frame.iter().enumerate() {
            self.history[channel * CAPACITY + self.filled] = *sample;
        }
        self.filled += 1;
    }

    /// Compute one output frame from the newest `TAPS` frames and the current phase row.
    fn emit(&self, frame: &mut [f32]) {
        let row = &self.coefficients[self.phase as usize * TAPS..][..TAPS];
        for (channel, out) in frame.iter_mut().enumerate() {
            let base = channel * CAPACITY + self.filled;
            *out = to_f32(dot(&self.history[base - TAPS..base], row));
        }
    }
}

/// Dot product in `f64`: 64 finite `f32` products cannot overflow it, and the result is rounded
/// once.
fn dot(window: &[f32], row: &[f32]) -> f64 {
    let (window, _) = window.as_chunks::<4>();
    let (row, _) = row.as_chunks::<4>();
    let mut sums = [0.0f64; 4];
    for (w, c) in window.iter().zip(row) {
        for ((sum, w), c) in sums.iter_mut().zip(w).zip(c) {
            *sum += f64::from(*w) * f64::from(*c);
        }
    }
    (sums[0] + sums[1]) + (sums[2] + sums[3])
}

/// Round to `f32`, saturating instead of overflowing to infinity, so finite input always gives
/// finite output.
fn to_f32(value: f64) -> f32 {
    (value as f32).clamp(-f32::MAX, f32::MAX)
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Modified Bessel function of the first kind, order zero (power series).
fn bessel_i0(x: f64) -> f64 {
    let quarter = x * x / 4.0;
    let (mut term, mut sum) = (1.0f64, 1.0f64);
    for k in 1..200 {
        term *= quarter / (f64::from(k) * f64::from(k));
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

/// Kaiser window with the edge pedestal removed, zero outside `|u| <= 1`; `norm` is
/// `bessel_i0(beta) - 1`.
fn window(u: f64, beta: f64, norm: f64) -> f64 {
    if u.abs() >= 1.0 {
        return 0.0;
    }
    (bessel_i0(beta * (1.0 - u * u).sqrt()) - 1.0) / norm
}

fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        1.0
    } else {
        let a = std::f64::consts::PI * x;
        a.sin() / a
    }
}

/// The `phases * TAPS` coefficient table for `from -> to`; see the module documentation.
fn design(from: u32, to: u32, phases: u32) -> Box<[f32]> {
    let nyquist = 0.5 * f64::from(from.min(to)) / f64::from(from);
    let half_transition = HALF_TRANSITION.min(0.25 * nyquist);
    let beta = REFERENCE_BETA * half_transition / HALF_TRANSITION;
    let cutoff = nyquist - half_transition;
    let norm = bessel_i0(beta) - 1.0;
    let mut table = Vec::with_capacity(phases as usize * TAPS);
    let mut row = [0.0f64; TAPS];
    for phase in 0..phases {
        let fraction = f64::from(phase) / f64::from(phases);
        for (tap, value) in row.iter_mut().enumerate() {
            // Tap `tap` is input index `i - (HALF - 1) + tap`; the output sits at `i + fraction`.
            let d = tap as f64 - (HALF as f64 - 1.0) - fraction;
            *value = 2.0 * cutoff * sinc(2.0 * cutoff * d) * window(d / HALF as f64, beta, norm);
        }
        let gain: f64 = row.iter().sum();
        table.extend(row.iter().map(|v| (v / gain) as f32));
    }
    table.into_boxed_slice()
}
