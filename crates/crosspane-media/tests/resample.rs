#![allow(clippy::unwrap_used, clippy::expect_used)]
//! WP-3.7a quality bar for `crosspane_media::audio::Resampler`.
//!
//! Measurements are printed with `eprintln!` (run with `--no-capture` to see them).

use crosspane_media::audio::{AudioError, Processed, Resampler};
use proptest::prelude::*;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    f64::consts::TAU,
};

// ---- counting allocator -------------------------------------------------------------------

thread_local! {
    // Const-initialised and destructor-free, so reading it inside the allocator never allocates.
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

struct Counting;

fn count_allocation() {
    ALLOCATIONS.with(|count| count.set(count.get() + 1));
}

// SAFETY: every method forwards its arguments unchanged to `System`, which upholds the
// `GlobalAlloc` contract; the only addition is a count in a thread-local `Cell`.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        // SAFETY: the caller's obligations for `alloc` are passed through to `System` unchanged.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        // SAFETY: the caller's obligations for `alloc_zeroed` are passed through unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count_allocation();
        // SAFETY: the caller's obligations for `realloc` are passed through unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's obligations for `dealloc` are passed through unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

fn allocations() -> usize {
    ALLOCATIONS.with(Cell::get)
}

// ---- helpers ------------------------------------------------------------------------------

const RATIOS: [(u32, u32); 3] = [(48_000, 44_100), (48_000, 96_000), (48_000, 88_200)];

fn sine(frequency: f64, rate: u32, frames: usize, amplitude: f64) -> Vec<f32> {
    (0..frames)
        .map(|n| (amplitude * (TAU * frequency * n as f64 / f64::from(rate)).sin()) as f32)
        .collect()
}

/// Deterministic noise in `[-1, 1]` (xorshift64*).
fn noise(seed: u64, samples: usize) -> Vec<f32> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..samples)
        .map(|_| {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let bits = state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40;
            bits as f32 / (1u64 << 23) as f32 - 1.0
        })
        .collect()
}

/// Everything one `process` call yields for `input`, with room for all of it.
fn resample(from: u32, to: u32, channels: u16, input: &[f32]) -> Vec<f32> {
    let mut resampler = Resampler::new(from, to, channels).unwrap();
    let channels = usize::from(channels);
    let frames = input.len() / channels;
    let mut output = vec![0.0; (frames * to as usize / from as usize + 8) * channels];
    let moved = resampler.process(input, &mut output);
    output.truncate(moved.produced * channels);
    output
}

fn rms(values: &[f32]) -> f64 {
    (values.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / values.len() as f64).sqrt()
}

fn db(ratio: f64) -> f64 {
    20.0 * ratio.log10()
}

/// Least-squares `p sin(wn) + q cos(wn)` fit, with `n` the absolute output index.
struct Fit {
    amplitude: f64,
    phase: f64,
    /// The sine's angular frequency per output frame.
    omega: f64,
}

fn fit_sine(output: &[f32], rate: u32, frequency: f64, skip: usize) -> Fit {
    let omega = TAU * frequency / f64::from(rate);
    let (mut ss, mut cc, mut sc, mut ys, mut yc) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for (n, y) in output.iter().enumerate().skip(skip) {
        let (s, c) = (omega * n as f64).sin_cos();
        let y = f64::from(*y);
        ss += s * s;
        cc += c * c;
        sc += s * c;
        ys += y * s;
        yc += y * c;
    }
    let determinant = ss * cc - sc * sc;
    let p = (ys * cc - yc * sc) / determinant;
    let q = (yc * ss - ys * sc) / determinant;
    Fit {
        amplitude: p.hypot(q),
        phase: q.atan2(p),
        omega,
    }
}

/// RMS of `y - reference_amplitude * sin(wn + phase)` relative to the reference's RMS, in dB.
fn residual_db(output: &[f32], fit: &Fit, reference_amplitude: f64, skip: usize) -> f64 {
    let residual: Vec<f32> = output
        .iter()
        .enumerate()
        .skip(skip)
        .map(|(n, y)| {
            (f64::from(*y) - reference_amplitude * (fit.omega * n as f64 + fit.phase).sin()) as f32
        })
        .collect();
    db(rms(&residual) / (reference_amplitude / 2f64.sqrt()))
}

/// A resampler that has already been fed `frames` frames of noise.
fn warmed(from: u32, to: u32, channels: u16, frames: usize) -> Resampler {
    let mut resampler = Resampler::new(from, to, channels).unwrap();
    let input = noise(7, frames * usize::from(channels));
    let mut output = vec![0.0; (frames * to as usize / from as usize + 8) * usize::from(channels)];
    resampler.process(&input, &mut output);
    resampler
}

/// Drive `input` through `resampler` with the given `(offered, capacity)` call sizes (frames),
/// carrying input the resampler did not consume, then flush with room for everything.
fn chunked(resampler: &mut Resampler, input: &[f32], splits: &[(usize, usize)]) -> Vec<f32> {
    let channels = usize::from(resampler.channels());
    let total = input.len() / channels;
    let (mut pending, mut output, mut offered) = (Vec::<f32>::new(), Vec::<f32>::new(), 0);
    let call = |resampler: &mut Resampler, pending: &mut Vec<f32>, capacity: usize| {
        let mut buffer = vec![0.0; capacity * channels];
        let before: Vec<usize> = (0..=capacity)
            .map(|frames| resampler.input_frames_for(frames))
            .collect();
        let moved = resampler.process(pending, &mut buffer);
        assert_eq!(moved.consumed, before[moved.produced]);
        pending.drain(..moved.consumed * channels);
        buffer[..moved.produced * channels].to_vec()
    };
    for &(offer, capacity) in splits {
        let offer = offer.min(total - offered);
        pending.extend_from_slice(&input[offered * channels..(offered + offer) * channels]);
        offered += offer;
        output.extend(call(resampler, &mut pending, capacity));
    }
    pending.extend_from_slice(&input[offered * channels..]);
    loop {
        let produced = call(resampler, &mut pending, 4096);
        if produced.is_empty() {
            break;
        }
        output.extend(produced);
    }
    output
}

// ---- quality bar --------------------------------------------------------------------------

#[test]
fn passband_is_flat_within_a_tenth_of_a_decibel() {
    for (from, to) in RATIOS {
        for frequency in [100.0, 1_000.0, 10_000.0, 19_000.0] {
            let input = sine(frequency, from, 24_000, 0.5);
            let output = resample(from, to, 1, &input);
            let fit = fit_sine(&output, to, frequency, 600);
            let gain = db(fit.amplitude / 0.5);
            eprintln!("passband {from}->{to} {frequency:>7} Hz: {gain:+.5} dB");
            assert!(gain.abs() <= 0.1, "{from}->{to} {frequency} Hz: {gain} dB");
        }
    }
}

#[test]
fn one_kilohertz_sine_is_reproduced_ninety_decibels_clean() {
    for (from, to) in RATIOS {
        let resampler = Resampler::new(from, to, 1).unwrap();
        let input = sine(1_000.0, from, 48_000, 0.5);
        let output = resample(from, to, 1, &input);
        let skip = 600;
        let fit = fit_sine(&output, to, 1_000.0, skip);
        let fixed = residual_db(&output, &fit, 0.5, skip);
        let free = residual_db(&output, &fit, fit.amplitude, skip);
        // The fitted phase is the delay: y[n] = A sin(w (n - d)), so d = -phase / w, modulo one
        // period of the tone.
        let period = TAU / fit.omega;
        let mut delay = -fit.phase / fit.omega;
        let latency = resampler.latency_frames() as f64;
        delay += ((latency - delay) / period).round() * period;
        eprintln!(
            "accuracy {from}->{to} 1 kHz: residual {fixed:.1} dB (amplitude fixed at 0.5), \
             {free:.1} dB (fitted amplitude); delay {delay:.3} output frames, latency_frames {}",
            resampler.latency_frames()
        );
        assert!(fixed <= -90.0, "{from}->{to}: residual {fixed} dB");
        assert!(
            (delay - latency).abs() <= 1.0,
            "{from}->{to}: delay {delay} vs latency_frames {latency}"
        );
    }
}

/// Input tones from just above the 44.1 kHz output's Nyquist frequency (22.05 kHz) up to just
/// below the 48 kHz input's Nyquist frequency all fold into the output band; each must come out
/// at least 67 dB down.
///
/// The measured stopband is 67.9 dB or better over this whole range (a 64-tap Kaiser-family
/// filter cannot do better there while staying within 0.1 dB up to 19 kHz). That is enough
/// because the stream is decoded from Opus fullband audio, which carries at most 20 kHz: there is
/// essentially no signal between 22.05 kHz and 24 kHz to alias. A dense sweep (25 Hz steps)
/// rather than a few tones keeps the bar honest: the response has notches (23.0 kHz is one, at
/// about -87 dB) that a lone tone could land on, and narrow peaks (near 22.66 kHz) that a coarse
/// sweep could miss. Two frequencies are left out on purpose: exactly 24 kHz samples to zero at
/// 48 kHz, and exactly 22.05 kHz aliases onto the output Nyquist frequency, where the RMS of a
/// sampled sine depends on its phase (up to +3 dB), so the sweep starts at 22.06 kHz.
#[test]
fn tones_between_output_nyquist_and_input_nyquist_are_attenuated_67_decibels() {
    let tones: Vec<f64> = std::iter::once(22_060.0)
        .chain((0..=75).map(|step| 22_075.0 + 25.0 * f64::from(step)))
        .collect();
    assert_eq!(tones.last(), Some(&23_950.0));
    let measured: Vec<(f64, f64)> = tones
        .iter()
        .map(|&frequency| {
            let input = sine(frequency, 48_000, 48_000, 0.5);
            let output = resample(48_000, 44_100, 1, &input);
            let attenuation = -db(rms(&output[600..]) / (0.5 / 2f64.sqrt()));
            eprintln!("alias 48000->44100 {frequency:>7} Hz: {attenuation:.1} dB down");
            (frequency, attenuation)
        })
        .collect();
    let (weakest_at, weakest) =
        measured
            .iter()
            .fold((0.0, f64::INFINITY), |worst, &(frequency, attenuation)| {
                if attenuation < worst.1 {
                    (frequency, attenuation)
                } else {
                    worst
                }
            });
    eprintln!("weakest attenuation: {weakest:.1} dB at {weakest_at} Hz");
    assert!(
        weakest >= 67.0,
        "{weakest_at} Hz comes out only {weakest} dB down"
    );
}

#[test]
fn identity_is_bit_exact_with_zero_latency() {
    for channels in [1u16, 2] {
        let mut resampler = Resampler::new(48_000, 48_000, channels).unwrap();
        assert!(resampler.is_identity());
        assert_eq!(resampler.latency_frames(), 0);
        assert_eq!(resampler.input_frames_for(123), 123);
        let input = noise(3, 1000 * usize::from(channels));
        let mut output = vec![0.0; input.len()];
        let mut done = 0;
        for chunk in [1usize, 0, 17, 480, 502] {
            let channels = usize::from(channels);
            let moved = resampler.process(
                &input[done * channels..(done + chunk) * channels],
                &mut output[done * channels..(done + chunk) * channels],
            );
            assert_eq!(
                moved,
                Processed {
                    consumed: chunk,
                    produced: chunk
                }
            );
            done += chunk;
        }
        assert_eq!(done, 1000);
        assert!(
            input
                .iter()
                .zip(&output)
                .all(|(a, b)| a.to_bits() == b.to_bits())
        );
        // A smaller output limits the copy, and `reset` changes nothing.
        resampler.reset();
        let mut short = vec![0.0; 10 * usize::from(channels)];
        let moved = resampler.process(&input, &mut short);
        assert_eq!((moved.consumed, moved.produced), (10, 10));
    }
}

#[test]
fn identical_rates_that_are_not_48k_are_identity_too() {
    let resampler = Resampler::new(44_100, 44_100, 2).unwrap();
    assert!(resampler.is_identity());
    assert_eq!(resampler.latency_frames(), 0);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn chunking_never_changes_the_output(
        (from, to) in prop::sample::select(vec![
            (48_000u32, 44_100u32),
            (44_100, 48_000),
            (48_000, 96_000),
            (96_000, 48_000),
            (48_000, 88_200),
            (48_000, 192_000),
            (192_000, 48_000),
            (48_000, 8_000),
            (8_000, 48_000),
            (48_000, 48_000),
        ]),
        channels in prop::sample::select(vec![1u16, 2]),
        samples in prop::collection::vec(-1.0f32..=1.0, 0..2_400),
        splits in prop::collection::vec((0usize..400, 0usize..400), 1..40),
    ) {
        let frames = samples.len() / usize::from(channels);
        let input = &samples[..frames * usize::from(channels)];
        let reference = resample(from, to, channels, input);
        let mut resampler = Resampler::new(from, to, channels).unwrap();
        let output = chunked(&mut resampler, input, &splits);
        prop_assert_eq!(output.len(), reference.len());
        prop_assert!(output.iter().zip(&reference).all(|(a, b)| a.to_bits() == b.to_bits()));
    }
}

#[test]
fn accounting_converges_with_less_than_a_frame_of_drift() {
    for (from, to) in [
        (48_000u32, 44_100u32),
        (44_100, 48_000),
        (48_000, 96_000),
        (96_000, 48_000),
        (48_000, 88_200),
        (48_000, 176_400),
        (48_000, 192_000),
        (192_000, 48_000),
        (48_000, 32_000),
    ] {
        let divisor = {
            let (mut a, mut b) = (u64::from(from), u64::from(to));
            while b != 0 {
                (a, b) = (b, a % b);
            }
            a
        };
        let (phases, step) = (u64::from(to) / divisor, u64::from(from) / divisor);
        let mut resampler = Resampler::new(from, to, 1).unwrap();
        let input = noise(11, 2 * from as usize);
        let mut offsets = noise(5, 4_000).into_iter();
        let (mut pending, mut fed) = (Vec::<f32>::new(), 0usize);
        let (mut consumed, mut produced) = (0u64, 0u64);
        let mut buffer = vec![0.0; 3_000];
        while fed < input.len() || !pending.is_empty() {
            let (a, b) = (offsets.next().unwrap(), offsets.next().unwrap());
            let offer = (((a + 1.0) * 700.0) as usize).min(input.len() - fed);
            let capacity = ((b + 1.0) * 1_400.0) as usize + 1;
            pending.extend_from_slice(&input[fed..fed + offer]);
            fed += offer;
            let before = resampler.input_frames_for(capacity);
            let moved = resampler.process(&pending, &mut buffer[..capacity]);
            assert!(moved.consumed <= before);
            pending.drain(..moved.consumed);
            consumed += moved.consumed as u64;
            produced += moved.produced as u64;
            // Output `n` needs `floor(n * M / L) + 1` input frames; nothing more is consumed.
            let expected = if produced == 0 {
                0
            } else {
                (produced - 1) * step / phases + 1
            };
            assert_eq!(consumed, expected, "{from}->{to}");
            let (c, p) = (consumed as f64, produced as f64);
            let ratio = f64::from(from) / f64::from(to);
            assert!((c - p * ratio).abs() <= ratio.max(1.0), "{from}->{to}");
            assert!(
                (p - c / ratio).abs() <= (1.0 / ratio).max(1.0),
                "{from}->{to}"
            );
            if offer == 0 && moved.produced == 0 && fed == input.len() {
                break;
            }
        }
        let ratio = consumed as f64 / produced as f64;
        eprintln!(
            "accounting {from}->{to}: consumed {consumed}, produced {produced}, \
             consumed/produced {ratio:.6} vs {:.6}",
            f64::from(from) / f64::from(to)
        );
        assert!((ratio - f64::from(from) / f64::from(to)).abs() < 1e-3);
    }
}

#[test]
fn input_frames_for_is_exact() {
    let pairs = [
        (48_000u32, 44_100u32),
        (44_100, 48_000),
        (48_000, 96_000),
        (96_000, 48_000),
        (48_000, 88_200),
        (48_000, 192_000),
        (48_000, 8_000),
        (8_000, 48_000),
    ];
    for (from, to) in pairs {
        for channels in [1u16, 2] {
            for warm in [0usize, 1, 7, 100, 333] {
                for out in [0usize, 1, 2, 3, 10, 147, 480, 1_000] {
                    let channels_n = usize::from(channels);
                    let mut exact = warmed(from, to, channels, warm);
                    let mut short = warmed(from, to, channels, warm);
                    let needed = exact.input_frames_for(out);
                    assert_eq!(needed, short.input_frames_for(out));
                    let input = noise(9, (needed + 50) * channels_n);
                    let mut buffer = vec![0.0; out * channels_n];
                    // Exactly the right amount makes exactly `out` frames and consumes it all.
                    let moved = exact.process(&input[..needed * channels_n], &mut buffer);
                    assert_eq!(
                        moved,
                        Processed {
                            consumed: needed,
                            produced: out
                        },
                        "{from}->{to} warm {warm} out {out}"
                    );
                    // Spare input is left alone.
                    let mut other = warmed(from, to, channels, warm);
                    let moved = other.process(&input, &mut buffer);
                    assert_eq!(
                        moved,
                        Processed {
                            consumed: needed,
                            produced: out
                        }
                    );
                    // One frame fewer cannot make them all.
                    if needed > 0 {
                        let moved = short.process(&input[..(needed - 1) * channels_n], &mut buffer);
                        assert!(moved.produced < out, "{from}->{to} warm {warm} out {out}");
                    }
                }
            }
        }
    }
}

#[test]
fn square_waves_and_extremes_stay_finite_and_bounded() {
    let pairs = [
        (48_000u32, 44_100u32),
        (48_000, 96_000),
        (48_000, 88_200),
        (96_000, 48_000),
        (192_000, 48_000),
        (384_000, 8_000),
        (8_000, 384_000),
    ];
    let mut worst: f32 = 0.0;
    for (from, to) in pairs {
        for half_period in [1usize, 2, 3, 24, 480] {
            let input: Vec<f32> = (0..3_000)
                .map(|n| {
                    if (n / half_period) % 2 == 0 {
                        1.0
                    } else {
                        -1.0
                    }
                })
                .collect();
            let output = resample(from, to, 1, &input);
            assert!(output.iter().all(|v| v.is_finite()), "{from}->{to}");
            let peak = output.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            worst = worst.max(peak);
            assert!(
                peak <= 1.5,
                "{from}->{to} half period {half_period}: peak {peak}"
            );
        }
        // Constant full scale settles to exactly full scale: every phase row has unity DC gain.
        let output = resample(from, to, 1, &[1.0; 3_000]);
        let settled = &output[output.len() / 2..];
        assert!(!settled.is_empty());
        assert!(
            settled.iter().all(|v| (v - 1.0).abs() < 1e-5),
            "{from}->{to}"
        );
    }
    eprintln!("largest square-wave output sample: {worst}");
    // Finite input never gives non-finite output, even at the edge of the float range.
    for (from, to) in pairs {
        let input: Vec<f32> = (0..2_000)
            .map(|n| if n % 3 == 0 { f32::MAX } else { f32::MIN })
            .collect();
        let output = resample(from, to, 1, &input);
        assert!(output.iter().all(|v| v.is_finite()), "{from}->{to}");
    }
}

#[test]
fn process_and_reset_do_not_allocate() {
    // The counter must see allocations at all, or the zero below proves nothing.
    let probe = allocations();
    let witness = Box::new([1u8; 100]);
    assert!(allocations() > probe && std::hint::black_box(&witness).len() == 100);
    let mut resamplers = [
        Resampler::new(48_000, 44_100, 2).unwrap(),
        Resampler::new(48_000, 96_000, 1).unwrap(),
        Resampler::new(48_000, 88_200, 2).unwrap(),
        Resampler::new(48_000, 48_000, 2).unwrap(),
    ];
    let input = noise(1, 2 * 5_000);
    let mut output = vec![0.0; 2 * 5_000];
    let mut moved = 0;
    let before = allocations();
    for resampler in &mut resamplers {
        let channels = usize::from(resampler.channels());
        // Many calls of assorted sizes, enough to wrap the internal history repeatedly.
        for (offered, capacity) in [
            (0usize, 0usize),
            (1, 1),
            (333, 7),
            (4_000, 3_000),
            (1_000, 0),
        ] {
            let moves = resampler.process(
                &input[..offered * channels],
                &mut output[..capacity * channels],
            );
            moved += moves.produced;
        }
        resampler.reset();
        moved += resampler.process(&input, &mut output).produced;
        let needed = resampler.input_frames_for(1_000);
        moved += needed;
    }
    let spent = allocations() - before;
    assert!(moved > 0);
    assert_eq!(spent, 0, "process and reset allocated {spent} time(s)");
}

#[test]
fn channels_are_independent_and_keep_their_order() {
    for (from, to) in RATIOS {
        let left = sine(1_000.0, from, 3_000, 0.5);
        let right = sine(3_300.0, from, 3_000, 0.25);
        let stereo: Vec<f32> = left
            .iter()
            .zip(&right)
            .flat_map(|(l, r)| [*l, *r])
            .collect();
        let output = resample(from, to, 2, &stereo);
        let mono_left = resample(from, to, 1, &left);
        let mono_right = resample(from, to, 1, &right);
        assert_eq!(output.len(), 2 * mono_left.len());
        for (frame, pair) in output.as_chunks::<2>().0.iter().enumerate() {
            assert_eq!(pair[0].to_bits(), mono_left[frame].to_bits());
            assert_eq!(pair[1].to_bits(), mono_right[frame].to_bits());
        }
        // Signal on one channel leaves the other exactly silent.
        let one_sided: Vec<f32> = left.iter().flat_map(|l| [*l, 0.0]).collect();
        let output = resample(from, to, 2, &one_sided);
        let (pairs, _) = output.as_chunks::<2>();
        assert!(
            pairs
                .iter()
                .all(|pair| pair[1] == 0.0 && pair[0].is_finite())
        );
        assert!(pairs.iter().any(|pair| pair[0] != 0.0));
    }
}

#[test]
fn group_delay_matches_latency_frames() {
    for (from, to, latency) in [
        (48_000u32, 44_100u32, 29usize),
        (48_000, 96_000, 64),
        (48_000, 88_200, 59),
        (96_000, 48_000, 16),
    ] {
        let resampler = Resampler::new(from, to, 1).unwrap();
        assert_eq!(resampler.latency_frames(), latency, "{from}->{to}");
        let impulse_at = 100;
        let mut input = vec![0.0f32; 400];
        input[impulse_at] = 1.0;
        let output = resample(from, to, 1, &input);
        let peak = output
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .unwrap()
            .0;
        let expected = impulse_at as f64 * f64::from(to) / f64::from(from) + latency as f64;
        eprintln!("impulse {from}->{to}: peak at output frame {peak}, expected {expected:.2}");
        assert!((peak as f64 - expected).abs() <= 1.0, "{from}->{to}");
    }
}

#[test]
fn reset_makes_a_fresh_stream() {
    for (from, to) in RATIOS {
        let input = noise(21, 2 * 3_000);
        let reference = resample(from, to, 2, &input);
        let mut resampler = warmed(from, to, 2, 777);
        resampler.reset();
        let mut output = vec![0.0; reference.len() + 16];
        let moved = resampler.process(&input, &mut output);
        output.truncate(moved.produced * 2);
        assert!(
            output
                .iter()
                .zip(&reference)
                .all(|(a, b)| a.to_bits() == b.to_bits())
        );
        assert_eq!(output.len(), reference.len());
    }
}

#[test]
fn standard_rate_pairs_pass_low_frequencies_flat() {
    let rates = [
        8_000u32, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000,
    ];
    let mut tested = 0;
    for from in rates {
        for to in rates {
            // Decimation beyond 2:1 narrows the pass band (64 taps); pairs needing more than
            // 1024 phases are refused by construction.
            if u64::from(to) * 2 < u64::from(from)
                || from == to
                || Resampler::new(from, to, 1).is_err()
            {
                continue;
            }
            tested += 1;
            for frequency in [100.0, 1_000.0] {
                let frames = (from as usize) / 4;
                let input = sine(frequency, from, frames, 0.5);
                let output = resample(from, to, 1, &input);
                let fit = fit_sine(&output, to, frequency, 600.min(output.len() / 4));
                let gain = db(fit.amplitude / 0.5);
                assert!(gain.abs() <= 0.1, "{from}->{to} {frequency} Hz: {gain} dB");
            }
        }
    }
    eprintln!("standard pairs checked: {tested}");
    assert!(tested >= 40);
}

// ---- construction -------------------------------------------------------------------------

#[test]
fn construction_validates_ranges_and_phase_count() {
    assert!(Resampler::new(8_000, 384_000, 1).is_ok());
    assert!(Resampler::new(384_000, 8_000, 2).is_ok());
    assert!(Resampler::new(48_000, 44_100, 2).is_ok());
    for (from, to, channels) in [
        (7_999u32, 48_000u32, 2u16),
        (48_000, 7_999, 2),
        (384_001, 48_000, 2),
        (48_000, 384_001, 2),
        (48_000, 44_100, 0),
        (48_000, 44_100, 3),
        (48_000, 47_999, 2),
        (0, 0, 2),
    ] {
        assert_eq!(
            Resampler::new(from, to, channels).err(),
            Some(AudioError::InvalidFormat),
            "{from}->{to} x{channels}"
        );
    }
}

#[test]
fn accessors_and_debug_do_not_expose_samples() {
    let mut resampler = Resampler::new(48_000, 44_100, 2).unwrap();
    assert_eq!(
        (
            resampler.from_rate(),
            resampler.to_rate(),
            resampler.channels()
        ),
        (48_000, 44_100, 2)
    );
    assert!(!resampler.is_identity());
    let secret = [0.123_456_79f32; 64];
    let mut out = [0.0f32; 64];
    resampler.process(&secret, &mut out);
    let text = format!("{resampler:?}");
    assert!(
        text.contains("48000") && text.contains("44100") && text.contains("147"),
        "{text}"
    );
    assert!(!text.contains("0.1234"), "{text}");
}
