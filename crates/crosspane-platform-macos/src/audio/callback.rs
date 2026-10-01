//! The two IOProcs the host runs, written as plain functions over [`IoBuffer`] lists so the real
//! validation runs unchanged under the fake HAL in the tests.
//!
//! Real-time rules, all kept here: no allocation, no lock, no blocking, no HAL call, no logging, no
//! retained buffer pointer. State shared with the owner thread is atomics only
//! ([`CallbackControl`]); the one ring end each callback owns sits in an exclusion cell.

use std::cell::UnsafeCell;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crosspane_platform::IoGate;
use rtrb::{Consumer, Producer};

use super::devices::OutputLayout;
use super::hal::{CallbackControl, IoBuffer, IoCallback, MAX_CALLBACK_FRAMES};

/// Bytes of a stereo f32 frame.
const STEREO_FRAME_BYTES: usize = 8;
/// The most bytes an output callback will blindly zero when it rejects a layout.
const MAX_ZERO_BYTES: usize = MAX_CALLBACK_FRAMES * STEREO_FRAME_BYTES * 2;

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
            && buffer.byte_size as usize / unit <= MAX_CALLBACK_FRAMES
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

/// The physical-output IOProc: plays the 50 ms ring the agent fills, silence on underrun, whole
/// frames only.
pub(super) struct PlaybackRender {
    control: CallbackControl,
    stop_requested: AtomicBool,
    ring: RingCell<Consumer<f32>>,
    gate: Arc<IoGate>,
    layout: OutputLayout,
    shutdown: Arc<AtomicBool>,
}

impl PlaybackRender {
    pub(super) fn new(
        ring: Consumer<f32>,
        gate: Arc<IoGate>,
        layout: OutputLayout,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            control: CallbackControl::default(),
            stop_requested: AtomicBool::new(false),
            ring: RingCell::new(ring),
            gate,
            layout,
            shutdown,
        }
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
    /// Copy as many whole frames as are buffered, up to `out`'s length. Returns frames written.
    fn fill_interleaved(&self, out: &mut [f32]) -> usize {
        self.ring
            .with(|consumer| {
                let frames = (consumer.slots() / 2).min(out.len() / 2);
                if frames == 0 {
                    return 0;
                }
                let Ok(chunk) = consumer.read_chunk(frames * 2) else {
                    return 0;
                };
                let (first, second) = chunk.as_slices();
                // The ring wraps at an arbitrary index, so a frame can straddle the two slices;
                // reading them as one sequence keeps L and R in order.
                for (dst, src) in out.iter_mut().zip(first.iter().chain(second)) {
                    *dst = finite(*src);
                }
                chunk.commit_all();
                frames
            })
            .unwrap_or(0)
    }

    fn fill_planar(&self, left: &mut [f32], right: &mut [f32]) -> usize {
        self.ring
            .with(|consumer| {
                let frames = (consumer.slots() / 2).min(left.len());
                if frames == 0 {
                    return 0;
                }
                let Ok(chunk) = consumer.read_chunk(frames * 2) else {
                    return 0;
                };
                let (first, second) = chunk.as_slices();
                for (index, src) in first.iter().chain(second).enumerate() {
                    let target = if index % 2 == 0 {
                        &mut left[index / 2]
                    } else {
                        &mut right[index / 2]
                    };
                    *target = finite(*src);
                }
                chunk.commit_all();
                frames
            })
            .unwrap_or(0)
    }
}
