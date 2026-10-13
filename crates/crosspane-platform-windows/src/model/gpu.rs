//! Pure admission and ownership state shared by the Windows GPU adapters.
//! Native support is unmeasured until W6.2. An uncertain handoff never permits readback/reuse.

use crosspane_types::geom::PixelSize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Disabled,
    Adapter,
    Removed,
    Sharing,
    Pending,
    Submitted,
    Gate,
    Mft,
    Busy,
    Layout,
}
impl Reason {
    pub fn name(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Adapter => "adapter_mismatch",
            Self::Removed => "device_removed",
            Self::Sharing => "sharing_unavailable",
            Self::Pending => "pending",
            Self::Submitted => "handoff_incomplete",
            Self::Gate => "capture_retired",
            Self::Mft => "mft_unavailable",
            Self::Busy => "pool_busy",
            Self::Layout => "invalid_layout",
        }
    }
}

/// What a refusal means for the whole GPU path, as opposed to the one capture it hit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The D3D12 or D3D11 device was removed or reset. A new device may work later.
    Device,
    /// A submitted handoff could not be confirmed. Everything bound to the old device stays
    /// refused; a fresh device carries no uncertainty.
    Handoff,
    /// This adapter cannot do the sharing. That does not change while the process runs.
    Unsupported,
}
impl Reason {
    /// True for refusals that are the node's own lifecycle (locked, unlocked, capture retired),
    /// which callers must see as `PlatformError::Locked` rather than as a backend failure.
    pub fn lifecycle(self) -> bool {
        matches!(self, Self::Gate)
    }
    /// The fault a reason stands for, or None when it is only a per-capture or lifecycle
    /// refusal. A gate or epoch refusal is the node locking or unlocking: it is expected, and
    /// it must never cost the process its GPU path.
    pub fn fault(self) -> Option<Fault> {
        match self {
            Self::Removed => Some(Fault::Device),
            Self::Submitted => Some(Fault::Handoff),
            Self::Adapter | Self::Sharing => Some(Fault::Unsupported),
            Self::Disabled | Self::Pending | Self::Gate | Self::Mft | Self::Busy | Self::Layout => {
                None
            }
        }
    }
}

/// How often a rebuild of the GPU state may be attempted.
pub const REBUILD_INTERVAL_MS: u64 = 30_000;
/// Consecutive unconfirmed handoffs that retire the GPU instead of just their own surface.
pub const STRIKE_LIMIT: u32 = 3;
/// Consecutive failed slot creations that mean sharing will not work on this adapter.
pub const SURFACE_FAILURE_LIMIT: u32 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retired {
    pub fault: Fault,
    pub reason: Reason,
    pub at_ms: u64,
}

/// Whether one GPU instance may still be used. Retirement is one-way for the instance: a
/// retired instance never serves again, so every surface, lease and encoder slot bound to it
/// stays refused. Recovery builds a new instance (see [`Rebuild`]).
#[derive(Debug, Default)]
pub struct Latch {
    retired: Option<Retired>,
    strikes: u32,
    surface_failures: u32,
}
impl Latch {
    /// Records the first cause. An `Unsupported` verdict replaces a recoverable one, because
    /// it forbids the rebuild that the earlier cause would have allowed.
    pub fn retire(&mut self, fault: Fault, reason: Reason, now_ms: u64) {
        let replace = match self.retired {
            None => true,
            Some(old) => old.fault != Fault::Unsupported && fault == Fault::Unsupported,
        };
        if replace {
            self.retired = Some(Retired {
                fault,
                reason,
                at_ms: now_ms,
            });
        }
    }
    pub fn retired(&self) -> Option<Retired> {
        self.retired
    }
    pub fn healthy(&self) -> Result<(), Reason> {
        self.retired.map_or(Ok(()), |retired| Err(retired.reason))
    }
    /// One handoff of one surface timed out. Returns true when this strike retired the GPU.
    pub fn strike(&mut self, now_ms: u64) -> bool {
        self.strikes = self.strikes.saturating_add(1);
        if self.strikes >= STRIKE_LIMIT {
            self.retire(Fault::Handoff, Reason::Submitted, now_ms);
            return true;
        }
        false
    }
    /// A handoff completed: the queue is moving, earlier timeouts were not a stall.
    pub fn settled(&mut self) {
        self.strikes = 0;
    }
    /// A slot could not be created. Returns true when this failure latched `Unsupported`.
    pub fn surface_failed(&mut self, now_ms: u64) -> bool {
        self.surface_failures = self.surface_failures.saturating_add(1);
        if self.surface_failures >= SURFACE_FAILURE_LIMIT {
            self.retire(Fault::Unsupported, Reason::Sharing, now_ms);
            return true;
        }
        false
    }
    pub fn surface_created(&mut self) {
        self.surface_failures = 0;
    }
}

/// Bounds how often a retired GPU is rebuilt.
#[derive(Debug, Default)]
pub struct Rebuild {
    last_ms: Option<u64>,
}
impl Rebuild {
    /// True when `retired` is a recoverable retirement and no attempt happened in the last
    /// [`REBUILD_INTERVAL_MS`]. A true answer counts as the attempt.
    pub fn permit(&mut self, retired: Option<Retired>, now_ms: u64) -> bool {
        let Some(retired) = retired else {
            return false;
        };
        if retired.fault == Fault::Unsupported {
            return false;
        }
        if self
            .last_ms
            .is_some_and(|last| now_ms.saturating_sub(last) < REBUILD_INTERVAL_MS)
        {
            return false;
        }
        self.last_ms = Some(now_ms);
        true
    }
}

pub fn admit(capture: Option<u64>, dx12: Option<u64>, removed: bool) -> Result<(), Reason> {
    if removed {
        return Err(Reason::Removed);
    }
    match (capture, dx12) {
        (Some(a), Some(b)) if a == b => Ok(()),
        _ => Err(Reason::Adapter),
    }
}

pub fn permitted(open: bool, epoch: u64, captured: u64) -> Result<(), Reason> {
    if open && epoch == captured {
        Ok(())
    } else {
        Err(Reason::Gate)
    }
}

pub fn texture_admit(size: PixelSize, limit: u32) -> Result<(), Reason> {
    if size.width == 0 || size.height == 0 || size.width > limit || size.height > limit {
        Err(Reason::Layout)
    } else {
        Ok(())
    }
}

/// Owns an unnamed share handle until the open/validation operation leaves its scope.
/// The native adapter supplies CloseHandle; pure tests supply an observable close function.
#[derive(Debug)]
pub struct SharedHandle<T: Copy> {
    value: T,
    close: fn(T),
}
impl<T: Copy> SharedHandle<T> {
    pub fn new(value: T, close: fn(T)) -> Self {
        Self { value, close }
    }
    pub fn get(&self) -> T {
        self.value
    }
}
impl<T: Copy> Drop for SharedHandle<T> {
    fn drop(&mut self) {
        (self.close)(self.value);
    }
}

/// A slot is returned only after both the CPU lease and foreign queue ownership end.
#[derive(Debug)]
pub struct Slot {
    pub generation: u64,
    pub wrapped: bool,
    pub done: u64,
    pub retired: bool,
}
impl Slot {
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            wrapped: false,
            done: 0,
            retired: false,
        }
    }
    pub fn wrap(&mut self, generation: u64) -> Result<bool, Reason> {
        if self.retired || self.generation != generation {
            return Err(Reason::Sharing);
        }
        let first = !self.wrapped;
        self.wrapped = true;
        Ok(first)
    }
    pub fn free(&self, only_pool: bool, completed: u64) -> bool {
        !self.retired && only_pool && completed != u64::MAX && completed >= self.done
    }
    pub fn retire(&mut self) {
        self.retired = true;
    }
}

#[derive(Debug, Default)]
pub struct Handoff {
    pub staged: bool,
    pub submitted: bool,
    pub complete: bool,
}
impl Handoff {
    pub fn stage(&mut self) -> Result<(), Reason> {
        if self.staged || self.submitted {
            return Err(Reason::Submitted);
        }
        self.staged = true;
        Ok(())
    }
    pub fn submitted(&mut self) {
        self.submitted = true;
    }
    pub fn finish(&mut self) {
        self.complete = true;
        self.staged = false;
    }
    pub fn cancel(&mut self) {
        self.staged = false;
    }
    pub fn readable(&self) -> bool {
        !self.submitted || self.complete
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plane {
    pub offset: u64,
    pub pitch: u32,
    pub width: u32,
    pub height: u32,
    pub bytes_per_pixel: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub size: PixelSize,
    pub y: Plane,
    pub uv: Plane,
    pub bytes: u64,
}
impl Layout {
    pub fn fits(&self, max_buffer: u64, max_binding: u64) -> bool {
        self.bytes <= max_buffer && self.bytes <= max_binding
    }

    pub fn validate(display: PixelSize, y: Plane, uv: Plane, bytes: u64) -> Result<Self, Reason> {
        let size = PixelSize::new(
            display.width.checked_add(1).ok_or(Reason::Layout)? & !1,
            display.height.checked_add(1).ok_or(Reason::Layout)? & !1,
        );
        if size.width == 0
            || size.height == 0
            || size.width > 16384
            || size.height > 16384
            || (y.width, y.height, y.bytes_per_pixel) != (size.width, size.height, 1)
            || (uv.width, uv.height, uv.bytes_per_pixel) != (size.width / 2, size.height / 2, 2)
        {
            return Err(Reason::Layout);
        }
        let end = |p: Plane| -> Result<u64, Reason> {
            let row = p
                .width
                .checked_mul(p.bytes_per_pixel)
                .ok_or(Reason::Layout)?;
            if !p.offset.is_multiple_of(512) || !p.pitch.is_multiple_of(256) || p.pitch < row {
                return Err(Reason::Layout);
            }
            p.offset
                .checked_add(
                    u64::from(p.pitch)
                        .checked_mul(u64::from(p.height))
                        .ok_or(Reason::Layout)?,
                )
                .ok_or(Reason::Layout)
        };
        let y_end = end(y)?;
        let uv_end = end(uv)?;
        if y_end > uv.offset || uv_end > bytes || bytes > 1024 * 1024 * 1024 {
            return Err(Reason::Layout);
        }
        Ok(Self { size, y, uv, bytes })
    }
}

/// MF ownership is separate from caller ownership and GPU completion.
#[derive(Debug)]
pub struct InputRelease {
    pub generation: u64,
    pub caller: bool,
    pub gpu: bool,
    pub native: bool,
}
impl InputRelease {
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            caller: false,
            gpu: false,
            native: false,
        }
    }
    pub fn callback(&mut self, generation: u64) {
        if self.generation == generation {
            self.native = true;
        }
    }
    pub fn claim(&mut self, generation: u64) -> Result<(), Reason> {
        if !self.free() || generation == self.generation {
            return Err(Reason::Busy);
        }
        *self = Self::new(generation);
        self.gpu = true;
        self.native = true; // No sample has been handed to MF in this acquisition yet.
        Ok(())
    }
    pub fn free(&self) -> bool {
        self.caller && self.gpu && self.native
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyStep {
    Shader,
    CopySource,
    CopyDest,
    Common,
    Signal,
}
pub fn copy_order(steps: &[CopyStep]) -> bool {
    let mut progress = CopyProgress::default();
    steps.iter().all(|step| progress.advance(*step).is_ok()) && progress.complete()
}
#[derive(Debug, Default)]
pub struct CopyProgress(usize);
impl CopyProgress {
    pub fn advance(&mut self, step: CopyStep) -> Result<(), Reason> {
        let expected = [
            CopyStep::Shader,
            CopyStep::CopySource,
            CopyStep::CopyDest,
            CopyStep::Common,
            CopyStep::Signal,
        ];
        if expected.get(self.0) != Some(&step) {
            return Err(Reason::Submitted);
        }
        self.0 += 1;
        Ok(())
    }
    pub fn complete(&self) -> bool {
        self.0 == 5
    }
}
pub fn mft_admit(same_adapter: bool, aware: bool, manager: bool) -> Result<(), Reason> {
    if same_adapter && aware && manager {
        Ok(())
    } else {
        Err(Reason::Mft)
    }
}

/// Metadata only: never pixels, hashes, input contents, or clipboard data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Path {
    pub hash: &'static str,
    pub encode: &'static str,
    pub mft: String,
    pub reason: &'static str,
}
impl Default for Path {
    fn default() -> Self {
        Self {
            hash: "cpu",
            encode: "not_started",
            mft: String::new(),
            reason: Reason::Pending.name(),
        }
    }
}
impl Path {
    pub fn selected(name: String) -> Self {
        Self {
            encode: if name.ends_with(" / GPU input") {
                "gpu_mf"
            } else {
                "cpu_mf"
            },
            mft: name,
            reason: "selected",
            ..Default::default()
        }
    }
    pub fn refused(&mut self, reason: Reason) {
        self.encode = "cpu_mf";
        self.reason = reason.name();
    }
    pub fn native(&self) -> bool {
        self.encode == "gpu_mf"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_and_epoch_refusals_are_lifecycle_not_gpu_faults() {
        assert_eq!(permitted(false, 3, 3), Err(Reason::Gate));
        assert_eq!(permitted(true, 4, 3), Err(Reason::Gate));
        assert_eq!(Reason::Gate.fault(), None);
        assert!(Reason::Gate.lifecycle());
        for reason in [
            Reason::Removed,
            Reason::Submitted,
            Reason::Sharing,
            Reason::Adapter,
        ] {
            assert!(!reason.lifecycle(), "{}", reason.name());
        }
        // Per-capture refusals never retire the device either.
        for reason in [
            Reason::Disabled,
            Reason::Pending,
            Reason::Mft,
            Reason::Busy,
            Reason::Layout,
        ] {
            assert_eq!(reason.fault(), None, "{}", reason.name());
        }
        assert_eq!(Reason::Removed.fault(), Some(Fault::Device));
        assert_eq!(Reason::Submitted.fault(), Some(Fault::Handoff));
        assert_eq!(Reason::Sharing.fault(), Some(Fault::Unsupported));
        assert_eq!(Reason::Adapter.fault(), Some(Fault::Unsupported));
    }

    #[test]
    fn a_retired_latch_stays_refused_and_names_its_first_cause() {
        let mut latch = Latch::default();
        assert_eq!(latch.healthy(), Ok(()));
        latch.retire(Fault::Device, Reason::Removed, 10);
        latch.retire(Fault::Handoff, Reason::Submitted, 20);
        assert_eq!(latch.healthy(), Err(Reason::Removed));
        assert_eq!(latch.retired().map(|r| r.at_ms), Some(10));
        // Nothing but a new instance clears it: strikes and settling do not.
        latch.settled();
        assert_eq!(latch.healthy(), Err(Reason::Removed));
    }

    #[test]
    fn unsupported_replaces_a_recoverable_cause_but_never_the_reverse() {
        let mut latch = Latch::default();
        latch.retire(Fault::Device, Reason::Removed, 1);
        latch.retire(Fault::Unsupported, Reason::Sharing, 2);
        assert_eq!(latch.retired().map(|r| r.fault), Some(Fault::Unsupported));
        latch.retire(Fault::Device, Reason::Removed, 3);
        assert_eq!(latch.retired().map(|r| r.fault), Some(Fault::Unsupported));
    }

    #[test]
    fn isolated_handoff_timeouts_do_not_retire_but_consecutive_ones_do() {
        let mut latch = Latch::default();
        for round in 0..10 {
            assert!(!latch.strike(round));
            assert!(!latch.strike(round));
            latch.settled();
        }
        assert_eq!(latch.healthy(), Ok(()));
        assert!(!latch.strike(100));
        assert!(!latch.strike(101));
        assert!(latch.strike(102));
        assert_eq!(latch.healthy(), Err(Reason::Submitted));
        assert_eq!(latch.retired().map(|r| r.fault), Some(Fault::Handoff));
    }

    #[test]
    fn repeated_slot_failures_latch_unsupported_and_a_success_resets_the_count() {
        let mut latch = Latch::default();
        for _ in 0..SURFACE_FAILURE_LIMIT - 1 {
            assert!(!latch.surface_failed(5));
        }
        latch.surface_created();
        for _ in 0..SURFACE_FAILURE_LIMIT - 1 {
            assert!(!latch.surface_failed(6));
        }
        assert_eq!(latch.healthy(), Ok(()));
        assert!(latch.surface_failed(7));
        assert_eq!(latch.healthy(), Err(Reason::Sharing));
        assert_eq!(latch.retired().map(|r| r.fault), Some(Fault::Unsupported));
    }

    #[test]
    fn rebuild_is_bounded_and_refused_for_unsupported_or_healthy_state() {
        let device = Some(Retired {
            fault: Fault::Device,
            reason: Reason::Removed,
            at_ms: 100,
        });
        let unsupported = Some(Retired {
            fault: Fault::Unsupported,
            reason: Reason::Sharing,
            at_ms: 100,
        });
        let mut rebuild = Rebuild::default();
        assert!(!rebuild.permit(None, 1_000));
        assert!(!rebuild.permit(unsupported, 1_000));
        assert!(rebuild.permit(device, 1_000));
        // A failed attempt still counts: no second try inside the interval.
        assert!(!rebuild.permit(device, 1_000 + REBUILD_INTERVAL_MS - 1));
        assert!(rebuild.permit(device, 1_000 + REBUILD_INTERVAL_MS));
        assert!(!rebuild.permit(device, 1_000 + REBUILD_INTERVAL_MS + 1));
        // Handoff retirements rebuild too: the old instance keeps the uncertainty.
        let handoff = Some(Retired {
            fault: Fault::Handoff,
            reason: Reason::Submitted,
            at_ms: 0,
        });
        assert!(rebuild.permit(handoff, 1_000 + 2 * REBUILD_INTERVAL_MS));
    }
}
