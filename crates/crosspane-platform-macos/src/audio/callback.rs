//! The two IOProcs the host runs, written as plain functions over [`IoBuffer`] lists so the real
//! validation runs unchanged under the fake HAL in the tests.
//!
//! Real-time rules, all kept here: no allocation, no lock, no blocking, no HAL call, no logging, no
//! retained buffer pointer. State shared with the owner thread is atomics only
//! ([`CallbackControl`]); the one ring end each callback owns, and for playback the resampler and
//! its preallocated scratch, sit in an exclusion cell.

use std::cell::UnsafeCell;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crosspane_media::audio::{AudioError, Resampler};
use crosspane_platform::IoGate;
use rtrb::{Consumer, Producer};

use super::devices::{OutputLayout, STREAM_RATE_HZ};
use super::hal::{CallbackControl, IoBuffer, IoCallback, MAX_CALLBACK_FRAMES};

/// Bytes of a stereo f32 frame.
const STEREO_FRAME_BYTES: usize = 8;
/// The most frames one buffer of a physical output cycle may carry. A physical device picks its own
/// IO buffer size, and the driver's [`MAX_CALLBACK_FRAMES`] limit belongs to the Crosspane virtual
/// devices only (the speaker forwarder keeps it). Cycles above [`SCRATCH_FRAMES`] are rendered in
/// chunks; anything above this ceiling is a bad cycle.
const MAX_PLAYBACK_FRAMES: usize = 16_384;
/// The most bytes `silence` will blindly zero in one buffer when a layout is rejected: twice the
/// largest valid physical stereo cycle, so a whole valid buffer and a slightly oversized one are
/// both cleared, while a garbage size is left alone. It never writes past the reported size.
const MAX_ZERO_BYTES: usize = MAX_PLAYBACK_FRAMES * STEREO_FRAME_BYTES * 2;
// A physical cycle is never smaller than a virtual-device one.
const _: () = assert!(MAX_PLAYBACK_FRAMES >= MAX_CALLBACK_FRAMES);

/// One owner for a ring end that a callback uses but the owner thread outlives. The atomic flag
/// makes access exclusive even if an IO thread ever re-entered or two sessions overlapped, so the
/// `UnsafeCell` is never aliased.
pub(super) struct RingCell<T> {
    busy: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: `value` is only touched inside `with`, which first acquires `busy` with an atomic swap
// and releases it afterwards, so at most one thread accesses it at a time. `T: Send` lets that
// thread differ between calls.
unsafe impl<T: Send> Sync for RingCell<T> {}

impl<T> RingCell<T> {
    pub(super) fn new(value: T) -> Self {
        Self {
            busy: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// Run `f` with exclusive access, or return `None` if somebody else holds it.
    pub(super) fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        if self.busy.swap(true, Ordering::Acquire) {
            return None;
        }
        // SAFETY: the swap above returned false, so this thread owns `busy` until the store below;
        // no other reference to `value` exists or can be created in that window.
        let result = f(unsafe { &mut *self.value.get() });
        self.busy.store(false, Ordering::Release);
        Some(result)
    }
}

impl<T> fmt::Debug for RingCell<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RingCell").finish_non_exhaustive()
    }
}

/// View a non-null, aligned buffer as f32 samples.
///
/// # Safety
///
/// `buffer.data` must be non-null, 4-byte aligned and valid for `buffer.byte_size` bytes.
unsafe fn samples<'a>(buffer: &IoBuffer) -> &'a [f32] {
    // SAFETY: guaranteed by the caller; any bit pattern is a valid f32.
    unsafe { std::slice::from_raw_parts(buffer.data.cast::<f32>(), buffer.byte_size as usize / 4) }
}

/// # Safety
///
/// As [`samples`], and the memory must be writable and not aliased.
unsafe fn samples_mut<'a>(buffer: &IoBuffer) -> &'a mut [f32] {
    // SAFETY: guaranteed by the caller; any bit pattern is a valid f32.
    unsafe {
        std::slice::from_raw_parts_mut(buffer.data.cast::<f32>(), buffer.byte_size as usize / 4)
    }
}

fn finite(sample: f32) -> f32 {
    if sample.is_finite() { sample } else { 0.0 }
}

fn aligned(buffer: &IoBuffer) -> bool {
    (buffer.data as usize).is_multiple_of(align_of::<f32>())
}

/// The half-open address range `[start, end)` of a buffer, `None` if it would wrap the address
/// space. Slices are never built over a range that fails this.
fn range(buffer: &IoBuffer) -> Option<(usize, usize)> {
    let start = buffer.data as usize;
    start
        .checked_add(buffer.byte_size as usize)
        .map(|end| (start, end))
}

/// True when two non-empty ranges share no address.
fn disjoint(a: (usize, usize), b: (usize, usize)) -> bool {
    a.1 <= b.0 || b.1 <= a.0
}

/// What the hidden speakers-loopback input buffers hold this cycle.
enum Input<'a> {
    /// Nothing to forward (no list, a disabled stream, or zero frames).
    Empty,
    /// Not the validated layout: one interleaved stereo float buffer of at most 4096 frames.
    Bad,
    /// Interleaved L R L R ... samples, a whole number of frames.
    Stereo(&'a [f32]),
}

/// # Safety
///
/// The buffers satisfy the contract of [`IoCallback::process`].
unsafe fn stereo_input(input: &[IoBuffer]) -> Input<'_> {
    let [buffer] = input else {
        return if input.is_empty() {
            Input::Empty
        } else {
            Input::Bad
        };
    };
    if buffer.channels != 2 {
        return Input::Bad;
    }
    if buffer.data.is_null() || buffer.byte_size == 0 {
        // A disabled stream reports its size with a null pointer; zero frames is a no-op.
        return Input::Empty;
    }
    let bytes = buffer.byte_size as usize;
    if !bytes.is_multiple_of(STEREO_FRAME_BYTES)
        || bytes / STEREO_FRAME_BYTES > MAX_CALLBACK_FRAMES
        || !aligned(buffer)
        || range(buffer).is_none()
    {
        return Input::Bad;
    }
    // SAFETY: non-null, aligned, whole frames, within the ceiling; validity per the caller.
    Input::Stereo(unsafe { samples(buffer) })
}

/// The hidden speakers-loopback IOProc: forwards what apps play into "Crosspane speakers" to the
/// agent's 50 ms ring, whole frames only.
pub(super) struct SpeakerForward {
    control: CallbackControl,
    ring: Arc<RingCell<Producer<f32>>>,
    gate: Arc<IoGate>,
    /// Set when the host is dropped: checked before any sample moves, so a callback that outlives
    /// a wedged owner thread still goes silent.
    shutdown: Arc<AtomicBool>,
}

impl SpeakerForward {
    pub(super) fn new(
        ring: Arc<RingCell<Producer<f32>>>,
        gate: Arc<IoGate>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            control: CallbackControl::default(),
            ring,
            gate,
            shutdown,
        }
    }
}

impl fmt::Debug for SpeakerForward {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpeakerForward")
            .field("control", &self.control)
            .finish_non_exhaustive()
    }
}

impl IoCallback for SpeakerForward {
    unsafe fn process(&self, input: &[IoBuffer], _output: &[IoBuffer]) {
        let Some(_admission) = self.control.enter() else {
            return;
        };
        if self.shutdown.load(Ordering::SeqCst) {
            self.control.disable();
            return;
        }
        // A latched (or otherwise disabled) registration never forwards again.
        if self.control.is_disabled() {
            return;
        }
        // The gate is checked before any sample is read, let alone retained. An observed closure
        // latches this registration silent at once, whether or not the gate reopens.
        if !self.gate.is_open() {
            self.control.latch_gate_closed();
            return;
        }
        // SAFETY: forwarded from this method's own contract.
        match unsafe { stereo_input(input) } {
            Input::Empty => self.control.note_cycle(0, 0),
            Input::Bad => self.control.mark_bad(),
            Input::Stereo(samples) => {
                let frames = samples.len() / 2;
                let moved = self
                    .ring
                    .with(|producer| {
                        // Only whole frames: the number of free slots is rounded down to a frame, so
                        // an overrun can never rotate the channels.
                        let fit = (producer.slots() / 2).min(frames);
                        if fit == 0 {
                            return 0;
                        }
                        match producer.write_chunk_uninit(fit * 2) {
                            Ok(chunk) => {
                                chunk.fill_from_iter(samples[..fit * 2].iter().map(|s| finite(*s)));
                                fit
                            }
                            Err(_) => 0,
                        }
                    })
                    .unwrap_or(0);
                self.control.note_cycle(moved, frames - moved);
            }
        }
    }

    fn control(&self) -> &CallbackControl {
        &self.control
    }
}

/// Where a physical output cycle must write.
enum Target<'a> {
    Empty,
    Bad,
    Interleaved(&'a mut [f32]),
    Planar(&'a mut [f32], &'a mut [f32]),
}

/// # Safety
///
/// The buffers satisfy the contract of [`IoCallback::process`] and are writable.
unsafe fn output_target(output: &[IoBuffer], layout: OutputLayout) -> Target<'_> {
    let usable = |buffer: &IoBuffer, unit: usize| {
        !buffer.data.is_null()
            && (buffer.byte_size as usize).is_multiple_of(unit)
            && buffer.byte_size as usize / unit <= MAX_PLAYBACK_FRAMES
            && aligned(buffer)
            && range(buffer).is_some()
    };
    match (layout, output) {
        (OutputLayout::Interleaved, [buffer]) if buffer.channels == 2 => {
            if buffer.data.is_null() || buffer.byte_size == 0 {
                Target::Empty
            } else if usable(buffer, STEREO_FRAME_BYTES) {
                // SAFETY: validated above; writable per the caller.
                Target::Interleaved(unsafe { samples_mut(buffer) })
            } else {
                Target::Bad
            }
        }
        (OutputLayout::Planar, [left, right])
            if left.channels == 1 && right.channels == 1 && left.byte_size == right.byte_size =>
        {
            if left.data.is_null() || right.data.is_null() || left.byte_size == 0 {
                Target::Empty
            } else if usable(left, 4)
                && usable(right, 4)
                && matches!((range(left), range(right)), (Some(l), Some(r)) if disjoint(l, r))
            {
                // SAFETY: validated above: both ranges are non-wrapping and share no address, so
                // the two `&mut` slices cannot alias; writable per the caller.
                let left = unsafe { samples_mut(left) };
                // SAFETY: as above.
                let right = unsafe { samples_mut(right) };
                Target::Planar(left, right)
            } else {
                Target::Bad
            }
        }
        (_, []) => Target::Empty,
        _ => Target::Bad,
    }
}

/// Zero every plausible buffer of a rejected layout so the device never plays stale memory.
///
/// # Safety
///
/// As [`IoCallback::process`].
pub(super) unsafe fn silence(output: &[IoBuffer]) {
    for buffer in output {
        let bytes = buffer.byte_size as usize;
        if !buffer.data.is_null() && bytes <= MAX_ZERO_BYTES && range(buffer).is_some() {
            // SAFETY: non-null and valid for `byte_size` writable bytes per the caller; bounded.
            unsafe { std::ptr::write_bytes(buffer.data.cast::<u8>(), 0, bytes) };
        }
    }
}

/// Output frames the playback scratch holds, and so the most one resampler call renders: a device
/// buffer is at most [`MAX_PLAYBACK_FRAMES`] frames, so a cycle is one chunk or two. The device's
/// maximum IO buffer size is not read at open (the HAL seam has no such query), hence the fixed
/// size.
const SCRATCH_FRAMES: usize = 8192;
/// Input frames of slack on top of the proportional share of a chunk: the resampler's filter is 64
/// taps and its phase carry is a few frames, both well inside this.
const FILTER_FRAMES: usize = 64;

/// What the playback IOProc owns besides its control block: the ring's consumer end, the
/// 48 kHz to device rate resampler and the preallocated scratch they work in. All of it is touched
/// only inside [`RingCell::with`], on the IO thread.
struct PlaybackState {
    ring: Consumer<f32>,
    resampler: Resampler,
    /// Interleaved stereo input offered to the resampler: finite samples only, never more than the
    /// resampler can use for one chunk of output.
    input: Box<[f32]>,
    /// Interleaved stereo output of one chunk for a planar device, split into its two buffers
    /// afterwards. Empty for an interleaved device, which the resampler writes into directly.
    mix: Box<[f32]>,
    /// Output frames per resampler call: the most `input` can feed and `mix` can hold.
    chunk: usize,
}

/// Resample as many frames as the ring and the resampler allow into `output` (interleaved stereo),
/// returning the frames produced. Reads at most `input_frames_for(wanted)` whole frames from the
/// ring, makes every sample finite on the way in, and commits to the ring exactly the frames the
/// resampler consumed: input that cannot yet contribute stays in the ring for the next call.
fn render_chunk(
    ring: &mut Consumer<f32>,
    resampler: &mut Resampler,
    input: &mut [f32],
    output: &mut [f32],
) -> usize {
    let wanted = output.len() / 2;
    let offered = resampler
        .input_frames_for(wanted)
        .min(ring.slots() / 2)
        .min(input.len() / 2);
    let Ok(chunk) = ring.read_chunk(offered * 2) else {
        return 0;
    };
    let (first, second) = chunk.as_slices();
    // The ring wraps at an arbitrary index, so a frame can straddle the two slices; the resampler
    // takes them as one sequence (its output does not depend on how the input is split), so L and R
    // stay in order.
    for (dst, src) in input.iter_mut().zip(first.iter().chain(second)) {
        *dst = finite(*src);
    }
    let done = resampler.process(&input[..offered * 2], output);
    chunk.commit(done.consumed * 2);
    done.produced
}

impl PlaybackState {
    /// Render into one interleaved buffer; returns the frames produced (the rest is the caller's
    /// to silence).
    fn render_interleaved(&mut self, out: &mut [f32]) -> usize {
        let Self {
            ring,
            resampler,
            input,
            chunk,
            ..
        } = self;
        let total = out.len() / 2;
        let mut done = 0;
        while done < total {
            let wanted = (total - done).min(*chunk);
            let produced = render_chunk(
                ring,
                resampler,
                input,
                &mut out[done * 2..(done + wanted) * 2],
            );
            done += produced;
            if produced < wanted {
                break;
            }
        }
        done
    }

    /// As [`render_interleaved`](Self::render_interleaved), into two one-channel buffers of the
    /// same length.
    fn render_planar(&mut self, left: &mut [f32], right: &mut [f32]) -> usize {
        let Self {
            ring,
            resampler,
            input,
            mix,
            chunk,
        } = self;
        let total = left.len().min(right.len());
        let mut done = 0;
        while done < total {
            // `mix` is sized for a planar device; the bound only keeps a misuse from slicing out
            // of range.
            let wanted = (total - done).min(*chunk).min(mix.len() / 2);
            if wanted == 0 {
                break;
            }
            let produced = render_chunk(ring, resampler, input, &mut mix[..wanted * 2]);
            let (frames, _) = mix[..produced * 2].as_chunks::<2>();
            for (index, [l, r]) in frames.iter().enumerate() {
                left[done + index] = *l;
                right[done + index] = *r;
            }
            done += produced;
            if produced < wanted {
                break;
            }
        }
        done
    }
}

/// The physical-output IOProc: plays the 50 ms ring the agent fills at the device's own rate
/// (resampling the 48 kHz stream, or copying it unchanged at 48 kHz), silence on underrun, whole
/// frames only.
pub(super) struct PlaybackRender {
    control: CallbackControl,
    stop_requested: AtomicBool,
    state: RingCell<PlaybackState>,
    gate: Arc<IoGate>,
    layout: OutputLayout,
    /// The device's rate in Hz.
    rate: u32,
    shutdown: Arc<AtomicBool>,
}

impl PlaybackRender {
    /// Build the render for a device running at `rate` Hz, resampling from the 48 kHz ring. All
    /// allocation happens here; the IOProc allocates nothing. Fails (`InvalidFormat`) for a rate the
    /// resampler does not accept.
    pub(super) fn new(
        ring: Consumer<f32>,
        gate: Arc<IoGate>,
        layout: OutputLayout,
        rate: u32,
        shutdown: Arc<AtomicBool>,
    ) -> Result<Self, AudioError> {
        Self::with_chunk(ring, gate, layout, rate, shutdown, SCRATCH_FRAMES)
    }

    /// As [`new`](Self::new) with an explicit scratch size in output frames.
    fn with_chunk(
        ring: Consumer<f32>,
        gate: Arc<IoGate>,
        layout: OutputLayout,
        rate: u32,
        shutdown: Arc<AtomicBool>,
        chunk: usize,
    ) -> Result<Self, AudioError> {
        let resampler = Resampler::new(STREAM_RATE_HZ, rate, 2)?;
        let chunk = chunk.max(1);
        // The input one chunk of output can need: its proportional share, rounded up, and slack.
        let share = (chunk as u64 * u64::from(resampler.from_rate()))
            .div_ceil(u64::from(resampler.to_rate()));
        let input_frames = usize::try_from(share)
            .map_err(|_| AudioError::InvalidFormat)?
            .saturating_add(FILTER_FRAMES);
        let mix_frames = match layout {
            OutputLayout::Interleaved => 0,
            OutputLayout::Planar => chunk,
        };
        Ok(Self {
            control: CallbackControl::default(),
            stop_requested: AtomicBool::new(false),
            state: RingCell::new(PlaybackState {
                ring,
                resampler,
                input: vec![0.0; input_frames * 2].into_boxed_slice(),
                mix: vec![0.0; mix_frames * 2].into_boxed_slice(),
                chunk,
            }),
            gate,
            layout,
            rate,
            shutdown,
        })
    }

    /// The handle's stop: silence now, retirement by the owner thread.
    pub(super) fn request_stop(&self) {
        self.stop_requested.store(true, Ordering::SeqCst);
        self.control.disable();
    }

    pub(super) fn stop_requested(&self) -> bool {
        self.stop_requested.load(Ordering::SeqCst)
    }
}

impl fmt::Debug for PlaybackRender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlaybackRender")
            .field("layout", &self.layout)
            .field("rate", &self.rate)
            .field("control", &self.control)
            .finish_non_exhaustive()
    }
}

impl IoCallback for PlaybackRender {
    unsafe fn process(&self, _input: &[IoBuffer], output: &[IoBuffer]) {
        let Some(_admission) = self.control.enter() else {
            // A re-entrant invocation (a HAL bug): never leave the device stale memory.
            // SAFETY: forwarded from this method's own contract.
            unsafe { silence(output) };
            return;
        };
        // The host is gone, or an observed gate closure latches: this handle stays silent for the
        // rest of its life.
        if self.shutdown.load(Ordering::SeqCst) {
            self.control.disable();
        } else if !self.gate.is_open() {
            self.control.latch_gate_closed();
        }
        // SAFETY: forwarded from this method's own contract.
        let target = unsafe { output_target(output, self.layout) };
        let live = !self.control.is_disabled();
        match target {
            Target::Empty => self.control.note_cycle(0, 0),
            Target::Bad => {
                // SAFETY: forwarded from this method's own contract.
                unsafe { silence(output) };
                self.control.mark_bad();
            }
            Target::Interleaved(out) => {
                let frames = out.len() / 2;
                let moved = if live { self.fill_interleaved(out) } else { 0 };
                out[moved * 2..].fill(0.0);
                self.control.note_cycle(moved, frames - moved);
            }
            Target::Planar(left, right) => {
                let frames = left.len();
                let moved = if live {
                    self.fill_planar(left, right)
                } else {
                    0
                };
                left[moved..].fill(0.0);
                right[moved..].fill(0.0);
                self.control.note_cycle(moved, frames - moved);
            }
        }
    }

    fn control(&self) -> &CallbackControl {
        &self.control
    }
}

impl PlaybackRender {
    /// Play as many whole frames as the ring holds into `out`, up to its length, at the device
    /// rate. Returns the output frames written; the caller silences the rest.
    fn fill_interleaved(&self, out: &mut [f32]) -> usize {
        self.state
            .with(|state| state.render_interleaved(out))
            .unwrap_or(0)
    }

    /// As [`fill_interleaved`](Self::fill_interleaved) for a planar device.
    fn fill_planar(&self, left: &mut [f32], right: &mut [f32]) -> usize {
        self.state
            .with(|state| state.render_planar(left, right))
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    //! The physical-playback IOProc at the device's own rate (WP-3.7b), driven over plain buffer
    //! lists as the HAL would: a 48 kHz ring in, the resampled device buffers out. The 48 kHz
    //! behaviour is covered bit-exactly by `tests/audio.rs` as well.

    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::f64::consts::TAU;
    use std::ops::Range;

    use rtrb::RingBuffer;

    use super::*;
    use crate::audio::hal::CallbackStats;

    const LAYOUTS: [OutputLayout; 2] = [OutputLayout::Interleaved, OutputLayout::Planar];
    /// The production ring: 50 ms of 48 kHz stereo.
    const RING_SAMPLES: usize = 4800;
    /// Every output sample starts as this, so a frame the callback forgets to write shows.
    const SENTINEL: f32 = 7.0;
    /// The physical-output ceiling the tests pin down, in frames per buffer (spelled out so a
    /// change to the production constant fails them).
    const CEILING: usize = 16_384;

    /// Frame `n` of the 48 kHz test signal: left a 1 kHz tone at 0.5, right a 2 kHz tone at 0.25.
    fn source_frame(n: usize) -> [f32; 2] {
        let t = n as f64 / 48_000.0;
        [
            (0.5 * (TAU * 1000.0 * t).sin()) as f32,
            (0.25 * (TAU * 2000.0 * t).sin()) as f32,
        ]
    }

    /// Interleaved source frames `frames`.
    fn source(frames: Range<usize>) -> Vec<f32> {
        frames.flat_map(source_frame).collect()
    }

    /// A 48 kHz ring of `capacity` samples whose read and write positions are already `advance`
    /// samples in, so what is pushed next can wrap.
    fn ring(capacity: usize, advance: usize) -> (Producer<f32>, Consumer<f32>) {
        let (mut producer, mut consumer) = RingBuffer::new(capacity);
        for _ in 0..advance {
            producer.push(0.0).unwrap();
        }
        for _ in 0..advance {
            consumer.pop().unwrap();
        }
        (producer, consumer)
    }

    fn buffer(channels: u32, samples: &mut [f32]) -> IoBuffer {
        IoBuffer {
            channels,
            byte_size: (samples.len() * 4) as u32,
            data: samples.as_mut_ptr().cast(),
        }
    }

    /// What the resampler alone makes of `input`: the exact stream the IOProc must produce.
    fn reference(rate: u32, input: &[f32], out_frames: usize) -> Vec<f32> {
        let mut resampler = Resampler::new(STREAM_RATE_HZ, rate, 2).unwrap();
        let mut out = vec![0.0; out_frames * 2];
        let done = resampler.process(input, &mut out);
        assert_eq!(done.produced, out_frames, "the reference ran out of input");
        out
    }

    struct Rig {
        render: PlaybackRender,
        producer: Producer<f32>,
        /// Ring capacity in samples.
        capacity: usize,
        layout: OutputLayout,
        /// The next source frame to push.
        next: usize,
    }

    impl Rig {
        fn new(layout: OutputLayout, rate: u32) -> Self {
            Self::build(layout, rate, SCRATCH_FRAMES, RING_SAMPLES, 0)
        }

        fn build(
            layout: OutputLayout,
            rate: u32,
            chunk: usize,
            capacity: usize,
            advance: usize,
        ) -> Self {
            let (producer, consumer) = ring(capacity, advance);
            Self::from_ring(producer, consumer, layout, rate, chunk, capacity)
        }

        fn from_ring(
            producer: Producer<f32>,
            consumer: Consumer<f32>,
            layout: OutputLayout,
            rate: u32,
            chunk: usize,
            capacity: usize,
        ) -> Self {
            let gate = IoGate::new();
            gate.set_session_permits(true);
            gate.set_engine_permits(true);
            let render = PlaybackRender::with_chunk(
                consumer,
                gate,
                layout,
                rate,
                Arc::new(AtomicBool::new(false)),
                chunk,
            )
            .unwrap();
            Self {
                render,
                producer,
                capacity,
                layout,
                next: 0,
            }
        }

        fn push_samples(&mut self, samples: &[f32]) {
            for sample in samples {
                self.producer.push(*sample).unwrap();
            }
        }

        /// Push source frames `frames` and remember where the next push continues.
        fn push(&mut self, frames: Range<usize>) {
            self.push_samples(&source(frames.clone()));
            self.next = frames.end;
        }

        /// Fill the ring with the next source frames.
        fn top_up(&mut self) {
            let room = self.producer.slots() / 2;
            self.push(self.next..self.next + room);
        }

        /// Whole frames waiting in the ring.
        fn queued(&self) -> usize {
            (self.capacity - self.producer.slots()) / 2
        }

        fn stats(&self) -> CallbackStats {
            self.render.control().stats()
        }

        fn run(&self, list: &[IoBuffer]) {
            // SAFETY: every buffer in `list` points at live, writable memory of the stated size that
            // the caller owns for the duration of this call, and no two overlap.
            unsafe { self.render.process(&[], list) };
        }

        /// One IO cycle of `frames` output frames; the result is interleaved whatever the layout.
        fn cycle(&self, frames: usize) -> Vec<f32> {
            match self.layout {
                OutputLayout::Interleaved => {
                    let mut out = vec![SENTINEL; frames * 2];
                    self.run(&[buffer(2, &mut out)]);
                    out
                }
                OutputLayout::Planar => {
                    let mut left = vec![SENTINEL; frames];
                    let mut right = vec![SENTINEL; frames];
                    self.run(&[buffer(1, &mut left), buffer(1, &mut right)]);
                    left.iter()
                        .zip(&right)
                        .flat_map(|(l, r)| [*l, *r])
                        .collect()
                }
            }
        }

        /// The cycles, each preceded by a top-up of the ring from the source; the output joined.
        fn stream(&mut self, cycles: &[usize]) -> Vec<f32> {
            let mut out = Vec::new();
            for frames in cycles {
                self.top_up();
                out.extend(self.cycle(*frames));
            }
            assert_eq!(self.stats().frames_dropped, 0, "the ring ran dry");
            out
        }
    }

    fn channel(interleaved: &[f32], index: usize) -> Vec<f64> {
        interleaved
            .iter()
            .skip(index)
            .step_by(2)
            .map(|s| f64::from(*s))
            .collect()
    }

    /// Amplitude of the component at `frequency` (exact for whole cycles in `x`).
    fn amplitude_at(x: &[f64], frequency: f64, rate: f64) -> f64 {
        let step = TAU * frequency / rate;
        let (mut re, mut im) = (0.0, 0.0);
        for (n, value) in x.iter().enumerate() {
            let angle = step * n as f64;
            re += value * angle.cos();
            im -= value * angle.sin();
        }
        2.0 * (re * re + im * im).sqrt() / x.len() as f64
    }

    /// Rising zero crossings (linearly interpolated) and the frequency they imply.
    fn zero_crossing_frequency(x: &[f64], rate: f64) -> (usize, f64) {
        let times: Vec<f64> = x
            .windows(2)
            .enumerate()
            .filter(|(_, pair)| pair[0] < 0.0 && pair[1] >= 0.0)
            .map(|(i, pair)| i as f64 + -pair[0] / (pair[1] - pair[0]))
            .collect();
        let span = times.last().unwrap() - times.first().unwrap();
        (times.len(), (times.len() - 1) as f64 * rate / span)
    }

    fn decibels(ratio: f64) -> f64 {
        20.0 * ratio.log10()
    }

    #[test]
    fn a_one_khz_tone_comes_out_as_a_one_khz_tone_at_44_1_khz() {
        for layout in LAYOUTS {
            let mut rig = Rig::new(layout, 44_100);
            let output = rig.stream(&[512; 100]);
            // 44_100 frames hold exactly 1000 cycles of the left tone and 2000 of the right one,
            // whatever the phase; skip the filter's latency and start-up first.
            let window = 1_000..1_000 + 44_100;
            let left = channel(&output, 0)[window.clone()].to_vec();
            let right = channel(&output, 1)[window].to_vec();

            let (crossings, frequency) = zero_crossing_frequency(&left, 44_100.0);
            let level = decibels(amplitude_at(&left, 1000.0, 44_100.0) / 0.5);
            eprintln!(
                "44.1 kHz {layout:?}: left tone {frequency:.4} Hz ({crossings} rising zero crossings in 1 s), \
                 {level:+.4} dB re 0.5"
            );
            assert!(
                (frequency - 1000.0).abs() < 0.05,
                "{layout:?}: {frequency} Hz"
            );
            assert!(crossings.abs_diff(1000) <= 1, "{layout:?}: {crossings}");
            assert!(level.abs() < 0.2, "{layout:?}: {level} dB");

            // The channels are independent: the right one is its own 2 kHz tone.
            let (crossings, frequency) = zero_crossing_frequency(&right, 44_100.0);
            let level = decibels(amplitude_at(&right, 2000.0, 44_100.0) / 0.25);
            assert!(
                (frequency - 2000.0).abs() < 0.1,
                "{layout:?}: {frequency} Hz"
            );
            assert!(crossings.abs_diff(2000) <= 1, "{layout:?}: {crossings}");
            assert!(level.abs() < 0.2, "{layout:?}: {level} dB");
            assert!(amplitude_at(&left, 2000.0, 44_100.0) < 5e-4, "{layout:?}");
            assert!(amplitude_at(&right, 1000.0, 44_100.0) < 5e-4, "{layout:?}");
        }
    }

    #[test]
    fn consumed_input_tracks_the_rate_ratio_of_the_produced_output() {
        for layout in LAYOUTS {
            for frames in [1usize, 64, 441, 480, 512, 1000, 2000] {
                let mut rig = Rig::new(layout, 44_100);
                let mut produced = 0;
                for cycle in 0..60 {
                    rig.top_up();
                    rig.cycle(frames);
                    produced += frames;
                    let consumed = rig.next - rig.queued();
                    let expected = produced as f64 * 48_000.0 / 44_100.0;
                    assert!(
                        (consumed as f64 - expected).abs() <= 2.0,
                        "{layout:?} {frames} frames, cycle {cycle}: consumed {consumed}, \
                         expected {expected:.2}"
                    );
                }
                assert_eq!(rig.stats().frames_moved, produced as u64);
                assert_eq!(rig.stats().frames_dropped, 0);
            }
        }
    }

    #[test]
    fn a_ring_that_wraps_mid_cycle_plays_the_same_as_one_that_does_not() {
        for layout in LAYOUTS {
            // An odd advance in an even-sized ring puts the wrap point inside a frame.
            let (mut producer, mut consumer) = ring(1000, 301);
            for sample in source(0..400) {
                producer.push(sample).unwrap();
            }
            {
                let chunk = consumer.read_chunk(800).unwrap();
                let (first, second) = chunk.as_slices();
                assert!(
                    !first.len().is_multiple_of(2) && !second.is_empty(),
                    "the ring must wrap inside a frame"
                );
            }
            let mut wrapped =
                Rig::from_ring(producer, consumer, layout, 44_100, SCRATCH_FRAMES, 1000);
            let mut plain = Rig::build(layout, 44_100, SCRATCH_FRAMES, 1000, 0);
            plain.push(0..400);
            wrapped.next = 400;
            let (mut a, mut b) = (Vec::new(), Vec::new());
            for frames in [150, 100, 90] {
                a.extend(wrapped.cycle(frames));
                b.extend(plain.cycle(frames));
            }
            assert_eq!(a, b, "{layout:?}");
            assert!(a.iter().any(|s| s.abs() > 0.1), "{layout:?}: not silence");
            assert_eq!(wrapped.stats(), plain.stats());
            assert_eq!(wrapped.stats().frames_dropped, 0);
            assert_eq!(wrapped.queued(), plain.queued());
        }
    }

    #[test]
    fn an_underrun_plays_what_the_ring_allows_then_silence_and_the_stream_continues() {
        for layout in LAYOUTS {
            let mut rig = Rig::new(layout, 44_100);
            rig.push(0..100);
            let first = rig.cycle(200);
            let stats = rig.stats();
            let produced = stats.frames_moved as usize;
            assert!(produced > 80 && produced < 100, "{layout:?}: {produced}");
            assert_eq!(stats.frames_dropped as usize, 200 - produced);
            assert_eq!(stats.callbacks, 1);
            assert!(first[produced * 2..].iter().all(|s| *s == 0.0));
            // Input that could not contribute yet stays in the ring.
            let fresh = Resampler::new(STREAM_RATE_HZ, 44_100, 2).unwrap();
            assert_eq!(100 - rig.queued(), fresh.input_frames_for(produced));

            // The data comes back: the stream continues where the resampler left off.
            rig.push(100..400);
            let second = rig.cycle(100);
            assert_eq!(rig.stats().frames_moved as usize, produced + 100);
            assert_eq!(rig.stats().frames_dropped as usize, 200 - produced);
            let mut played = first[..produced * 2].to_vec();
            played.extend(&second);
            assert_eq!(played, reference(44_100, &source(0..400), produced + 100));
        }
    }

    #[test]
    fn input_that_cannot_yet_contribute_stays_in_the_ring() {
        for layout in LAYOUTS {
            for rate in [44_100, 88_200, 96_000, 192_000] {
                let fresh = Resampler::new(STREAM_RATE_HZ, rate, 2).unwrap();
                let mut left_over = false;
                for available in 40..160 {
                    let mut rig = Rig::new(layout, rate);
                    rig.push(0..available);
                    let out = rig.cycle(2000);
                    let produced = rig.stats().frames_moved as usize;
                    let consumed = available - rig.queued();
                    let case = format!("{layout:?} {rate} Hz, {available} frames");
                    // Exactly what the produced frames needed, and as many frames as the input
                    // allowed.
                    assert_eq!(consumed, fresh.input_frames_for(produced), "{case}");
                    assert!(fresh.input_frames_for(produced + 1) > available, "{case}");
                    left_over |= consumed < available;
                    let expected = reference(rate, &source(0..available), produced);
                    assert_eq!(out[..produced * 2], expected[..], "{case}");
                    assert!(out[produced * 2..].iter().all(|s| *s == 0.0), "{case}");
                }
                // Decimating by a non-integer ratio sometimes needs two more frames for the next
                // output, so one is left unconsumed.
                assert!(left_over || rate != 44_100, "{layout:?} {rate}");
            }
        }
    }

    #[test]
    fn an_empty_ring_is_a_cycle_of_silence_with_the_counts_of_what_was_missing() {
        for layout in LAYOUTS {
            for rate in [44_100, 48_000, 96_000] {
                let rig = Rig::new(layout, rate);
                let out = rig.cycle(64);
                assert!(out.iter().all(|s| *s == 0.0), "{layout:?} {rate}");
                let stats = rig.stats();
                assert_eq!(stats.frames_moved + stats.frames_dropped, 64);
                assert_eq!(stats.callbacks, 1);
            }
        }
    }

    #[test]
    fn planar_and_interleaved_agree_and_match_the_resampler() {
        for rate in [44_100, 88_200, 96_000, 176_400, 192_000] {
            let cycles = [37, 480, 129, 512, 1, 300, 64, 1000];
            let total: usize = cycles.iter().sum();
            let mut interleaved = Rig::new(OutputLayout::Interleaved, rate);
            let mut planar = Rig::new(OutputLayout::Planar, rate);
            let (a, b) = (interleaved.stream(&cycles), planar.stream(&cycles));
            assert_eq!(a, b, "{rate}");
            assert_eq!(interleaved.stats(), planar.stats(), "{rate}");
            assert_eq!(interleaved.queued(), planar.queued(), "{rate}");
            assert_eq!(
                a,
                reference(rate, &source(0..interleaved.next), total),
                "{rate}"
            );
        }
    }

    #[test]
    fn non_finite_samples_never_reach_the_output() {
        for layout in LAYOUTS {
            for rate in [44_100, 48_000] {
                let mut input = source(0..600);
                input[200] = f32::NAN;
                input[301] = f32::INFINITY;
                input[302] = f32::NEG_INFINITY;
                input[600] = f32::NAN;
                input[601] = f32::NAN;
                let mut rig = Rig::build(layout, rate, SCRATCH_FRAMES, 2400, 0);
                rig.push_samples(&input);
                let mut out = Vec::new();
                for _ in 0..8 {
                    out.extend(rig.cycle(64));
                }
                assert!(out.iter().all(|s| s.is_finite()), "{layout:?} {rate}");
                let clean: Vec<f32> = input
                    .iter()
                    .map(|s| if s.is_finite() { *s } else { 0.0 })
                    .collect();
                assert_eq!(out, reference(rate, &clean, 512), "{layout:?} {rate}");
                assert_eq!(rig.stats().frames_dropped, 0);
            }
        }
    }

    #[test]
    fn a_buffer_larger_than_the_scratch_is_rendered_in_chunks_that_join_exactly() {
        for layout in LAYOUTS {
            for rate in [8_000, 44_100, 48_000, 96_000, 192_000] {
                // A 64-frame scratch against 500-frame buffers: eight chunks per cycle.
                let mut small = Rig::build(layout, rate, 64, 20_000, 0);
                let mut big = Rig::build(layout, rate, SCRATCH_FRAMES, 20_000, 0);
                let cycles = [500, 500, 77, 1000];
                let (a, b) = (small.stream(&cycles), big.stream(&cycles));
                assert_eq!(a, b, "{layout:?} {rate}");
                assert_eq!(small.stats(), big.stats(), "{layout:?} {rate}");
                assert_eq!(small.queued(), big.queued(), "{layout:?} {rate}");
                assert_eq!(a, reference(rate, &source(0..small.next), 2077), "{rate}");

                // The ring runs dry inside a later chunk: the same frames play and the same
                // remain.
                let mut small = Rig::build(layout, rate, 64, 20_000, 0);
                let mut big = Rig::build(layout, rate, SCRATCH_FRAMES, 20_000, 0);
                // Half of the input 500 output frames need.
                let have = (500 * 48_000 / rate as usize) / 2;
                small.push(0..have);
                big.push(0..have);
                let (a, b) = (small.cycle(500), big.cycle(500));
                assert_eq!(a, b, "{layout:?} {rate} underrun");
                assert_eq!(small.stats(), big.stats(), "{layout:?} {rate} underrun");
                assert_eq!(small.queued(), big.queued(), "{layout:?} {rate} underrun");
                let moved = small.stats().frames_moved as usize;
                assert!(moved > 0 && moved < 500, "{rate}: {moved}");
                assert!(a[moved * 2..].iter().all(|s| *s == 0.0));
            }
        }
    }

    /// Run one cycle of `frames` per buffer in the rig's layout over a buffer list of the HAL's
    /// shape, and return the raw buffers afterwards (one interleaved or two planar).
    fn cycle_buffers(rig: &Rig, frames: usize) -> Vec<Vec<f32>> {
        let (channels, mut buffers) = match rig.layout {
            OutputLayout::Interleaved => (2, vec![vec![SENTINEL; frames * 2]]),
            OutputLayout::Planar => (1, vec![vec![SENTINEL; frames]; 2]),
        };
        let list: Vec<IoBuffer> = buffers
            .iter_mut()
            .map(|samples| buffer(channels, samples))
            .collect();
        rig.run(&list);
        buffers
    }

    #[test]
    fn physical_buffers_up_to_the_ceiling_render_in_chunks_with_the_production_scratch() {
        for layout in LAYOUTS {
            for rate in [22_050, 44_100, 48_000, 96_000, 192_000] {
                for cycles in [&[8192][..], &[8193], &[16384], &[8193, 16384, 8192]] {
                    // `Rig::build` with the production scratch, as `PlaybackRender::new` makes it.
                    let mut rig = Rig::build(layout, rate, SCRATCH_FRAMES, 80_000, 0);
                    let out = rig.stream(cycles);
                    let total: usize = cycles.iter().sum();
                    let case = format!("{layout:?} {rate} Hz {cycles:?}");
                    assert_eq!(out, reference(rate, &source(0..rig.next), total), "{case}");
                    assert_eq!(rig.stats().frames_moved as usize, total, "{case}");
                    assert!(!rig.render.control().bad_buffer_seen(), "{case}");
                    assert!(!rig.render.control().is_disabled(), "{case}");
                }
            }
        }
    }

    #[test]
    fn a_physical_buffer_above_the_ceiling_is_a_bad_cycle_that_is_silenced() {
        for layout in LAYOUTS {
            let mut rig = Rig::new(layout, 44_100);
            rig.push(0..1000);
            let buffers = cycle_buffers(&rig, CEILING + 1);
            assert!(rig.render.control().bad_buffer_seen(), "{layout:?}");
            assert!(rig.render.control().is_disabled(), "{layout:?}");
            assert!(
                buffers.iter().flatten().all(|s| *s == 0.0),
                "{layout:?}: stale memory left"
            );
            assert_eq!(rig.queued(), 1000, "{layout:?}: the ring is untouched");
        }
    }

    #[test]
    fn an_implausibly_large_buffer_is_bad_and_left_alone() {
        for layout in LAYOUTS {
            // Each buffer beyond the bytes `silence` will zero: refused, never written.
            let unit = match layout {
                OutputLayout::Interleaved => STEREO_FRAME_BYTES,
                OutputLayout::Planar => 4,
            };
            let rig = Rig::new(layout, 44_100);
            let buffers = cycle_buffers(&rig, MAX_ZERO_BYTES / unit + 1);
            assert!(rig.render.control().bad_buffer_seen(), "{layout:?}");
            assert!(
                buffers.iter().flatten().all(|s| *s == SENTINEL),
                "{layout:?}: written"
            );
        }
    }

    #[test]
    fn a_disabled_or_retired_playback_zeroes_a_full_physical_buffer() {
        for layout in LAYOUTS {
            for retired in [false, true] {
                let mut rig = Rig::new(layout, 44_100);
                rig.push(0..2000);
                if retired {
                    rig.render.request_stop();
                } else {
                    rig.render.control().disable();
                }
                let buffers = cycle_buffers(&rig, CEILING);
                let case = format!("{layout:?} retired={retired}");
                assert!(buffers.iter().flatten().all(|s| *s == 0.0), "{case}");
                assert!(!rig.render.control().bad_buffer_seen(), "{case}");
                let stats = rig.stats();
                assert_eq!(stats.frames_moved, 0, "{case}");
                assert_eq!(stats.frames_dropped as usize, CEILING, "{case}");
                assert_eq!(rig.queued(), 2000, "{case}: nothing consumed");
            }
        }
    }

    #[test]
    fn silence_clears_a_whole_physical_buffer_and_never_writes_past_its_size() {
        let frames = CEILING;
        // Interleaved: one stereo buffer, then a guard of untouched samples.
        let mut storage = vec![SENTINEL; frames * 2 + 8];
        let list = [IoBuffer {
            channels: 2,
            byte_size: (frames * STEREO_FRAME_BYTES) as u32,
            data: storage.as_mut_ptr().cast(),
        }];
        // SAFETY: the buffer is live, writable and `byte_size` bytes long.
        unsafe { silence(&list) };
        assert!(storage[..frames * 2].iter().all(|s| *s == 0.0));
        assert!(storage[frames * 2..].iter().all(|s| *s == SENTINEL));

        // Planar: two one-channel buffers with a guard each.
        let mut left = vec![SENTINEL; frames + 4];
        let mut right = vec![SENTINEL; frames + 4];
        let list = [&mut left, &mut right].map(|samples| IoBuffer {
            channels: 1,
            byte_size: (frames * 4) as u32,
            data: samples.as_mut_ptr().cast(),
        });
        // SAFETY: two live, separate, writable buffers of exactly `byte_size` bytes.
        unsafe { silence(&list) };
        for samples in [&left, &right] {
            assert!(samples[..frames].iter().all(|s| *s == 0.0));
            assert!(samples[frames..].iter().all(|s| *s == SENTINEL));
        }
    }

    #[test]
    fn the_scratch_always_covers_a_full_chunk_at_every_supported_rate() {
        for rate in [
            8_000, 11_025, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400,
            192_000, 384_000,
        ] {
            for layout in LAYOUTS {
                // Chunks of 256 output frames against cycles of every size around it; the ring
                // always holds more than a cycle needs, so any shortfall is the scratch's.
                let mut rig = Rig::build(layout, rate, 256, 20_000, 0);
                rig.stream(&[256, 256, 1000, 33, 256, 700, 255, 257, 512]);
                assert_eq!(rig.stats().frames_dropped, 0, "{layout:?} {rate}");
            }
        }
    }

    #[test]
    fn at_48_khz_the_output_is_the_input_bit_exactly() {
        for layout in LAYOUTS {
            for chunk in [SCRATCH_FRAMES, 33] {
                let mut rig = Rig::build(layout, 48_000, chunk, RING_SAMPLES, 0);
                let out = rig.stream(&[480; 10]);
                assert_eq!(out, source(0..4800), "{layout:?} chunk {chunk}");
                assert_eq!(rig.stats().frames_moved, 4800);
            }
            let mut rig = Rig::new(layout, 48_000);
            rig.push(0..3);
            let mut expected = source(0..3);
            expected.extend([0.0; 4]);
            assert_eq!(rig.cycle(5), expected, "{layout:?}");
            assert_eq!(rig.stats().frames_moved, 3);
            assert_eq!(rig.stats().frames_dropped, 2);
        }
    }

    #[test]
    fn a_rate_the_resampler_does_not_accept_is_refused_at_construction() {
        for rate in [0, 7_999, 384_001, 47_999] {
            let (_, consumer) = ring(100, 0);
            let built = PlaybackRender::new(
                consumer,
                IoGate::new(),
                OutputLayout::Interleaved,
                rate,
                Arc::new(AtomicBool::new(false)),
            );
            assert!(built.is_err(), "{rate}");
        }
    }
}
