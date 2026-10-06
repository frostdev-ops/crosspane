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
