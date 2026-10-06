//! OS-free realtime H.264 parameter, pixel and event contracts.

use crosspane_media::{
    codec::CodecError,
    picture::{Nv12, YuvColour},
};
use crosspane_types::geom::PixelSize;

/// The truthful receiving path; the GPU path copies pixels only on the GPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MfDecodePath {
    CpuNv12,
    DxgiGpuCopy,
    AwaitingCpuIdr,
}
impl MfDecodePath {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CpuNv12 => "cpu_nv12",
            Self::DxgiGpuCopy => "mf_dxgi_gpu_copy",
            Self::AwaitingCpuIdr => "awaiting_cpu_idr",
        }
    }
}
/// Classified adapter failures only; never driver/window/resource text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MfGpuFallback {
    HostNotReady,
    UnsupportedDevice,
    DeviceMismatch,
    UnsupportedMft,
    InvalidSurface,
    PoolBusy,
    CopyFailed,
    DeviceLost,
    Deadline,
}
impl MfGpuFallback {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HostNotReady => "host_not_ready",
            Self::UnsupportedDevice => "unsupported_device",
            Self::DeviceMismatch => "device_mismatch",
            Self::UnsupportedMft => "unsupported_mft",
            Self::InvalidSurface => "invalid_surface",
            Self::PoolBusy => "pool_busy",
            Self::CopyFailed => "copy_failed",
            Self::DeviceLost => "device_lost",
            Self::Deadline => "deadline",
        }
    }
}
pub const GPU_SOURCE_LEASES: usize = 8;
pub const GPU_COPY_JOBS: usize = 8;
pub const GPU_BOUND: std::time::Duration = std::time::Duration::from_secs(2);
pub const GPU_POLL: std::time::Duration = std::time::Duration::from_millis(5);

/// Native metadata, including the MF surface-array slice, not a guessed plane index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nv12CopyPlan {
    pub storage: PixelSize,
    pub slice: u32,
    pub layers: u32,
    pub area: Aperture,
}
impl Nv12CopyPlan {
    pub fn new(
        storage: PixelSize,
        layers: u32,
        slice: u32,
        area: Aperture,
        expected: Aperture,
    ) -> Result<Self, MfGpuFallback> {
        if layers == 0
            || layers > 64
            || slice >= layers
            || area != expected
            || area.validate().is_err()
            || Params::new(storage, 1, 1).and_then(Params::coded).ok() != Some(storage)
            || area
                .x
                .checked_add(area.size.width)
                .is_none_or(|n| n > storage.width)
            || area
                .y
                .checked_add(area.size.height)
                .is_none_or(|n| n > storage.height)
        {
            return Err(MfGpuFallback::InvalidSurface);
        }
        Ok(Self {
            storage,
            slice,
            layers,
            area,
        })
    }
    /// Plane origins/extents are in texels, including chroma subsampling.
    pub fn plane(self, plane: u32) -> Option<((u32, u32), PixelSize)> {
        match plane {
            0 => Some(((self.area.x, self.area.y), self.area.size)),
            1 => Some((
                (self.area.x / 2, self.area.y / 2),
                PixelSize::new(self.area.size.width / 2, self.area.size.height / 2),
            )),
            _ => None,
        }
    }
}

/// Host observer updates are correlated by generation, including late retirement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuHostUpdate {
    Ready,
    RetireCurrent,
    RetireOther,
    Ignore,
}
pub fn gpu_host_update(
    latest: Option<u64>,
    current: Option<u64>,
    generation: u64,
    ready: bool,
    retired: bool,
) -> GpuHostUpdate {
    if ready {
        if retired
            || latest.is_some_and(|latest| generation < latest)
            || current == Some(generation)
        {
            GpuHostUpdate::Ignore
        } else {
            GpuHostUpdate::Ready
        }
    } else if current == Some(generation) {
        GpuHostUpdate::RetireCurrent
    } else {
        GpuHostUpdate::RetireOther
    }
}

/// Retirement is terminal for one picture; a late successful import cannot overwrite it.
pub fn gpu_path_update(
    current: Option<(MfDecodePath, Option<MfGpuFallback>)>,
    next: (MfDecodePath, Option<MfGpuFallback>),
) -> Option<(MfDecodePath, Option<MfGpuFallback>)> {
    if current.is_some_and(|(path, _)| path == MfDecodePath::AwaitingCpuIdr) {
        current
    } else {
        Some(next)
    }
}

/// One output is reserved BEFORE publication or the next MFT input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpuLeaseState {
    pub submitted: bool,
    pub cancelled: bool,
    pub completion: bool,
    pub returned: bool,
    pub picture_dropped: bool,
    pub quarantined: bool,
}
impl Default for GpuLeaseState {
    fn default() -> Self {
        Self::reserved()
    }
}
impl GpuLeaseState {
    pub const fn reserved() -> Self {
        Self {
            submitted: false,
            cancelled: false,
            completion: false,
            returned: false,
            picture_dropped: false,
            quarantined: false,
        }
    }
    pub fn next_input_allowed(self) -> bool {
        self.returned && !self.quarantined
    }
    pub fn release_sample(self) -> bool {
        self.returned && self.picture_dropped && !self.quarantined
    }
    pub fn check_in_ready(self, ready_completed: bool) -> bool {
        !self.returned
            && if self.submitted {
                self.completion
            } else {
                self.cancelled && ready_completed
            }
    }
    pub fn submit(&mut self) -> Result<(), MfGpuFallback> {
        if self.submitted || self.cancelled || self.returned || self.quarantined {
            return Err(MfGpuFallback::CopyFailed);
        }
        self.submitted = true;
        Ok(())
    }
    /// Retirement cannot undo successful native COMMON/check-in proof.
    pub fn quarantine(&mut self) {
        if !self.returned {
            self.quarantined = true;
        }
    }
    pub fn return_completed(&mut self) {
        self.returned = true;
        self.quarantined = false;
    }
}

/// A timeout stops active progress without falsely settling or recycling native storage.
pub fn gpu_poll_deadline(
    now: std::time::Instant,
    deadlines: impl IntoIterator<Item = std::time::Instant>,
) -> Option<std::time::Instant> {
    deadlines
        .into_iter()
        .filter(|at| *at > now)
        .min()
        .map(|at| at.min(now + GPU_POLL))
}

/// Capability admission uses exact device identity, not adapter identity alone.
pub fn gpu_decode_admission(
    current: Option<u64>,
    generation: u64,
    same_device: bool,
    nv12: bool,
) -> Result<(), MfGpuFallback> {
    match current {
        None => Err(MfGpuFallback::HostNotReady),
        Some(current) if current != generation || !same_device => {
            Err(MfGpuFallback::DeviceMismatch)
        }
        Some(_) if !nv12 => Err(MfGpuFallback::UnsupportedDevice),
        Some(_) => Ok(()),
    }
}
pub fn gpu_capacity(occupied: usize, limit: usize) -> Result<(), MfGpuFallback> {
    if occupied >= limit {
        Err(MfGpuFallback::PoolBusy)
    } else {
        Ok(())
    }
}
/// Each frame receives independently owned outputs; there is no mutating reuse operation.
pub fn allocate_gpu_planes<T>(
    size: PixelSize,
    mut allocate: impl FnMut(bool, PixelSize) -> T,
) -> [T; 2] {
    [
        allocate(false, size),
        allocate(true, PixelSize::new(size.width / 2, size.height / 2)),
    ]
}
/// Reading an uncached native picture is unavailable, not a request to the GPU/MFT.
pub fn native_cpu_cache(cache: Option<&Nv12>, out: &mut Nv12) -> Result<(), CodecError> {
    let cache = cache
        .ok_or_else(|| CodecError::Unavailable("native MF picture has no CPU cache".into()))?;
    cache
        .validate()
        .map_err(|_| failed("invalid native CPU cache"))?;
    *out = cache.clone();
    Ok(())
}

/// Settle all return/drop controls before deciding whether any next native input can run.
pub fn gpu_drain_control<T>(
    leases: &mut Vec<T>,
    mut settled: impl FnMut(&mut T) -> bool,
    blocks: impl Fn(&T) -> bool,
) -> bool {
    leases.retain_mut(|lease| !settled(lease));
    leases.iter().any(blocks)
}
/// Promotion/rebinding is never performed on a dependent P access unit.
pub fn gpu_idr_binding(idr: bool, generation: Option<u64>) -> Option<u64> {
    generation.filter(|_| idr)
}

pub const GOP: u32 = 100_000;
pub const MAX_DIMENSION: u32 = 8192;

/// Lazy attempts preserve enumeration order; software runs only if every candidate refuses.
pub fn prefer_hardware<T>(
    attempts: impl IntoIterator<Item = Result<T, CodecError>>,
    software: impl FnOnce() -> Result<T, CodecError>,
) -> Result<T, CodecError> {
    attempts
        .into_iter()
        .find_map(Result::ok)
        .map_or_else(software, Ok)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    pub size: PixelSize,
    pub bitrate: u32,
    pub fps: u32,
}
impl Params {
    pub fn new(size: PixelSize, bitrate: u32, fps: u32) -> Result<Self, CodecError> {
        if size.width == 0 || size.height == 0 || bitrate == 0 || fps == 0 || fps > 10_000_000 {
            return Err(CodecError::BadInput("invalid video parameters"));
        }
        let p = Self { size, bitrate, fps };
        p.coded()?;
        Ok(p)
    }
    pub fn coded(self) -> Result<PixelSize, CodecError> {
        let width = self.size.width.checked_add(1).map(|n| n & !1);
        let height = self.size.height.checked_add(1).map(|n| n & !1);
        match (width, height) {
            (Some(width), Some(height))
                if width > 0 && height > 0 && width <= MAX_DIMENSION && height <= MAX_DIMENSION =>
            {
                Ok(PixelSize::new(width, height))
            }
            _ => Err(CodecError::BadInput("invalid video dimensions")),
        }
    }
}
/// Pad an odd edge by repeating its last pixel; ignore BGRA alpha and row padding.
/// BT.709 limited range matches the media type signalled by the native encoder.
pub fn bgra_to_nv12(pixels: &[u8], stride: u32, size: PixelSize) -> Result<Nv12, CodecError> {
    let coded = Params::new(size, 1, 1)?.coded()?;
    let row = size.width as usize * 4;
    let needed = (size.height as usize - 1)
        .checked_mul(stride as usize)
        .and_then(|n| n.checked_add(row));
    if (stride as usize) < row || needed.is_none_or(|n| pixels.len() < n) {
        return Err(CodecError::BadInput(
            "BGRA stride or buffer shorter than frame",
        ));
    }
    let width = coded.width as usize;
    let height = coded.height as usize;
    let rgb = |x: usize, y: usize| {
        let offset =
            y.min(size.height as usize - 1) * stride as usize + x.min(size.width as usize - 1) * 4;
        let b = f64::from(pixels[offset]);
        let g = f64::from(pixels[offset + 1]);
        let r = f64::from(pixels[offset + 2]);
        [r, g, b]
    };
    let sample = |v: f64| v.round().clamp(0.0, 255.0) as u8;
    let mut out = Nv12 {
        size: coded,
        y_stride: coded.width,
        uv_stride: coded.width,
        ..Default::default()
    };
    out.y.resize(width * height, 0);
    out.uv.resize(width * height / 2, 0);
    for y in 0..height {
        for x in 0..width {
            let [r, g, b] = rgb(x, y);
            out.y[y * width + x] =
                sample(16.0 + (0.2126 * r + 0.7152 * g + 0.0722 * b) * 219.0 / 255.0);
        }
    }
    for y in (0..height).step_by(2) {
        for x in (0..width).step_by(2) {
            let mut u = 0.0;
            let mut v = 0.0;
            for dy in 0..2 {
                for dx in 0..2 {
                    let [r, g, b] = rgb(x + dx, y + dy);
                    let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
                    u += (b - luma) / (2.0 * (1.0 - 0.0722));
                    v += (r - luma) / (2.0 * (1.0 - 0.2126));
                }
            }
            let offset = y / 2 * width + x;
            out.uv[offset] = sample(128.0 + u / 4.0 * 224.0 / 255.0);
            out.uv[offset + 1] = sample(128.0 + v / 4.0 * 224.0 / 255.0);
        }
    }
    Ok(out)
}
#[derive(Default, Debug)]
pub struct Clock(u64);
impl Clock {
    pub fn next(&mut self, fps: u32) -> Result<(i64, i64), CodecError> {
        if fps == 0 || fps > 10_000_000 {
            return Err(CodecError::BadInput("invalid frame rate"));
        }
        let next = self
            .0
            .checked_add(1)
            .ok_or_else(|| failed("timestamp overflow"))?;
        let at = u128::from(self.0) * 10_000_000 / u128::from(fps);
        let end = u128::from(next) * 10_000_000 / u128::from(fps);
        let at = i64::try_from(at).map_err(|_| failed("timestamp overflow"))?;
        let end = i64::try_from(end).map_err(|_| failed("timestamp overflow"))?;
        self.0 = next;
        Ok((at, end - at))
    }
}
pub fn failed(reason: &str) -> CodecError {
    CodecError::Failed(reason.into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Aperture {
    pub x: u32,
    pub y: u32,
    pub size: PixelSize,
}
impl Aperture {
    /// Public MFVideoArea blob layout; fractional or negative origins cannot represent NV12 rows.
    pub fn from_blob(blob: &[u8]) -> Result<Self, CodecError> {
        if blob.len() != 16 {
            return Err(failed("invalid aperture blob"));
        }
        if blob[0..2] != [0, 0] || blob[4..6] != [0, 0] {
            return Err(failed("fractional aperture origin"));
        }
        let x = i16::from_le_bytes([blob[2], blob[3]]);
        let y = i16::from_le_bytes([blob[6], blob[7]]);
        if x < 0 || y < 0 {
            return Err(failed("negative aperture origin"));
        }
        let area = Self {
            x: x as u32,
            y: y as u32,
            size: PixelSize::new(
                u32::from_le_bytes([blob[8], blob[9], blob[10], blob[11]]),
                u32::from_le_bytes([blob[12], blob[13], blob[14], blob[15]]),
            ),
        };
        area.validate()?;
        Ok(area)
    }
    fn validate(self) -> Result<(), CodecError> {
        if !self.x.is_multiple_of(2)
            || !self.y.is_multiple_of(2)
            || !Params::new(self.size, 1, 1)
                .and_then(Params::coded)
                .is_ok_and(|size| size == self.size)
        {
            return Err(failed("invalid even aperture"));
        }
        Ok(())
    }
}

/// Progressive 4:2:0 SPS crop geometry, without inferring dimensions from decoder allocation.
pub fn h264_aperture(data: &[u8]) -> Result<Aperture, CodecError> {
    let mut sps = nals(data).filter(|nal| nal[0] & 31 == 7);
    let nal = sps.next().ok_or_else(|| failed("missing geometry SPS"))?;
    if sps.next().is_some() || nal.len() > 4096 || nal[0] & 0x80 != 0 {
        return Err(failed("ambiguous or oversized geometry SPS"));
    }
    let rbsp = &nal[1..];
    let mut zeros = 0;
    for (index, byte) in rbsp.iter().copied().enumerate() {
        if zeros == 2 {
            if byte == 3 {
                if rbsp.get(index + 1).is_none_or(|next| *next > 3) {
                    return Err(failed("invalid SPS emulation prevention"));
                }
                zeros = 0;
                continue;
            }
            if byte <= 2 {
                return Err(failed("unescaped SPS byte sequence"));
            }
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    let parsed = || {
        let mut bits = Rbsp::new(rbsp);
        bits.sps_prefix()?;
        match bits.ue()? {
            0 => {
                if bits.ue()? > 12 {
                    return None;
                }
            }
            1 => {
                bits.read(1)?;
                bits.ue()?;
                bits.ue()?;
                let count = bits.ue()?;
                if count > 255 {
                    return None;
                }
                for _ in 0..count {
                    bits.ue()?;
                }
            }
            2 => (),
            _ => return None,
        }
        if bits.ue()? > 16 {
            return None;
        }
        bits.read(1)?;
        let width = bits.ue()?.checked_add(1)?.checked_mul(16)?;
        let height = bits.ue()?.checked_add(1)?.checked_mul(16)?;
        if width > MAX_DIMENSION || height > MAX_DIMENSION || bits.read(1)? != 1 {
            return None; // progressive 4:2:0 only
        }
        bits.read(1)?;
        let mut offsets = [0; 4];
        if bits.read(1)? != 0 {
            for offset in &mut offsets {
                *offset = bits.ue()?.checked_mul(2)?;
            }
        }
        Some(Aperture {
            x: offsets[0],
            y: offsets[2],
            size: PixelSize::new(
                width.checked_sub(offsets[0].checked_add(offsets[1])?)?,
                height.checked_sub(offsets[2].checked_add(offsets[3])?)?,
            ),
        })
    };
    let area = parsed().ok_or_else(|| failed("unsupported or invalid SPS geometry"))?;
    area.validate()?;
    Ok(area)
}

/// Separate native storage/stride from the SPS-validated, even coded image returned to the caller.
pub fn decoded_nv12(
    storage: PixelSize,
    stride: u32,
    aperture: Option<Aperture>,
    expected: Aperture,
    bytes: &[u8],
    colour: YuvColour,
) -> Result<Nv12, CodecError> {
    if !Params::new(storage, 1, 1)
        .and_then(Params::coded)
        .is_ok_and(|size| size == storage)
        || stride < storage.width
        || stride > MAX_DIMENSION * 4
        || !stride.is_multiple_of(2)
    {
        return Err(failed("invalid NV12 storage"));
    }
    expected.validate()?;
    let area = aperture.unwrap_or(Aperture {
        x: 0,
        y: 0,
        size: storage,
    });
    if area != expected
        || area
            .x
            .checked_add(area.size.width)
            .is_none_or(|n| n > storage.width)
        || area
            .y
            .checked_add(area.size.height)
            .is_none_or(|n| n > storage.height)
    {
        return Err(failed("missing, mismatched or uncontained coded aperture"));
    }
    let width = area.size.width as usize;
    let height = area.size.height as usize;
    let stride = stride as usize;
    let y_len = stride * storage.height as usize;
    let y_start = area.y as usize * stride + area.x as usize;
    let uv_start = y_len + area.y as usize / 2 * stride + area.x as usize;
    if bytes.len() < uv_start + stride * (height / 2 - 1) + width {
        return Err(failed("short NV12 output buffer"));
    }
    let mut out = Nv12 {
        size: area.size,
        y_stride: area.size.width,
        uv_stride: area.size.width,
        colour,
        ..Default::default()
    };
    for row in 0..height {
        out.y
            .extend_from_slice(&bytes[y_start + row * stride..][..width]);
    }
    for row in 0..height / 2 {
        out.uv
            .extend_from_slice(&bytes[uv_start + row * stride..][..width]);
    }
    Ok(out)
}

pub fn nals(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut remaining = data;
    std::iter::from_fn(move || {
        let start = remaining.windows(3).position(|b| b == [0, 0, 1])? + 3;
        remaining = &remaining[start..];
        let end = remaining
            .windows(3)
            .position(|b| b == [0, 0, 1])
            .unwrap_or(remaining.len());
        let nal = &remaining[..end];
        remaining = &remaining[end..];
        let length = nal.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
        Some(&nal[..length])
    })
}
/// Classification is safe before validation; malformed empty NALs never match.
pub fn has_nal(data: &[u8], kind: u8) -> bool {
    nals(data).any(|nal| nal.first().is_some_and(|header| header & 31 == kind))
}
pub fn annex_b(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])
}
#[derive(Default, Debug)]
pub struct Headers {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}
impl Headers {
    pub fn remember(&mut self, data: &[u8]) -> Result<(), CodecError> {
        if !annex_b(data) {
            return Err(failed("not Annex B"));
        }
        for nal in nals(data) {
            let header = nal.first().copied().ok_or_else(|| failed("empty NAL"))?;
            if header & 0x80 != 0 || header & 0x1f == 0 || header & 0x1f > 23 {
                return Err(failed("invalid NAL header"));
            }
            match header & 0x1f {
                7 => self.sps = Some(nal.to_vec()),
                8 => self.pps = Some(nal.to_vec()),
                _ => (),
            }
        }
        Ok(())
    }
    pub fn packet(&mut self, data: &[u8], forced: bool) -> Result<(Vec<u8>, bool), CodecError> {
        self.remember(data)?;
        let key = nals(data).any(|n| n[0] & 0x1f == 5);
        if !nals(data).any(|n| matches!(n[0] & 0x1f, 1 | 5)) || forced && !key {
            return Err(failed("no picture or forced IDR refused"));
        }
        let mut out = Vec::new();
        if key {
            let sps = self.sps.as_ref().ok_or_else(|| failed("IDR missing SPS"))?;
            let pps = self.pps.as_ref().ok_or_else(|| failed("IDR missing PPS"))?;
            let before = nals(data)
                .take_while(|n| n[0] & 0x1f != 5)
                .fold(0, |mask, n| {
                    mask | match n[0] & 0x1f {
                        7 => 1,
                        8 => 2,
                        _ => 0,
                    }
                });
            if before != 3 {
                for nal in [sps, pps] {
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(nal);
                }
            }
        }
        out.extend_from_slice(data);
        Ok((out, key))
    }
}
#[derive(Default, Debug)]
pub struct Events {
    inputs: u32,
    outputs: u32,
    pending: Option<i64>,
}
impl Events {
    pub fn need_input(&mut self) -> Result<(), CodecError> {
        self.inputs = self
            .inputs
            .checked_add(1)
            .filter(|n| *n <= 16)
            .ok_or_else(|| failed("excess input events"))?;
        Ok(())
    }
    pub fn have_output(&mut self) -> Result<(), CodecError> {
        if self.pending.is_none() || self.outputs != 0 {
            return Err(failed("unexpected output event"));
        }
        self.outputs = 1;
        Ok(())
    }
    pub fn can_submit(&self) -> bool {
        self.inputs > 0 && self.pending.is_none()
    }
    pub fn can_output(&self) -> bool {
        self.outputs > 0
    }
    pub fn submit(&mut self, at: i64) -> Result<(), CodecError> {
        if !self.can_submit() {
            return Err(failed("input without event credit"));
        }
        self.inputs -= 1;
        self.pending = Some(at);
        Ok(())
    }
    pub fn complete(&mut self, at: i64) -> Result<(), CodecError> {
        if self.pending != Some(at) || self.outputs != 1 {
            return Err(failed("out of order, missing or duplicate output"));
        }
        self.pending = None;
        self.outputs = 0;
        Ok(())
    }
}

/// Reject frame_num gaps rather than relying on an MFT's error concealment.
/// This is the same progressive, no-B-frame prefix check as the Linux backend;
/// complete syntax, reference lists and pixel decoding remain the native decoder's job.
#[derive(Clone, Debug)]
pub struct References {
    frame_num_bits: [Option<u8>; 32],
    pps_sps: [Option<usize>; 256],
    last: Option<(usize, u32)>,
}
impl Default for References {
    fn default() -> Self {
        Self {
            frame_num_bits: [None; 32],
            pps_sps: [None; 256],
            last: None,
        }
    }
}
impl References {
    pub fn check(&mut self, data: &[u8]) -> Result<(), CodecError> {
        let mut next = self.clone();
        if Headers::default().packet(data, false).is_err() || next.prefixes(data).is_none() {
            *self = Self::default();
            return Err(failed("invalid H.264 headers or missing reference"));
        }
        *self = next;
        Ok(())
    }
    fn prefixes(&mut self, data: &[u8]) -> Option<()> {
        let mut picture = None;
        for nal in nals(data) {
            let header = *nal.first()?;
            let mut bits = Rbsp::new(&nal[1..]);
            match header & 0x1f {
                7 => {
                    let (sps, count) = bits.sps_prefix()?;
                    *self.frame_num_bits.get_mut(sps)? = Some(count);
                }
                8 => {
                    let pps = bits.ue()? as usize;
                    let sps = bits.ue()? as usize;
                    self.frame_num_bits.get(sps)?;
                    *self.pps_sps.get_mut(pps)? = Some(sps);
                }
                1 | 5 => {
                    bits.ue()?;
                    let slice = bits.ue()?;
                    if slice > 9 || slice % 5 == 1 {
                        return None;
                    }
                    let pps = bits.ue()? as usize;
                    let sps = self.pps_sps.get(pps).copied().flatten()?;
                    let count = self.frame_num_bits.get(sps).copied().flatten()?;
                    let number = bits.read(count)?;
                    let current = (sps, number, count, header & 0x1f == 5, header & 0x60 != 0);
                    if picture.is_some_and(|previous| previous != current) {
                        return None;
                    }
                    picture = Some(current);
                }
                _ => (),
            }
        }
        let (sps, number, count, idr, reference) = picture?;
        if idr {
            if number != 0 || !reference {
                return None;
            }
        } else {
            let (previous_sps, previous) = self.last?;
            if previous_sps != sps || number != (previous + 1) % (1 << count) {
                return None;
            }
        }
        if reference {
            self.last = Some((sps, number));
        }
        Some(())
    }
}
struct Rbsp<'a> {
    bytes: std::slice::Iter<'a, u8>,
    byte: u8,
    left: u8,
    zeros: u8,
}
impl<'a> Rbsp<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes: bytes.iter(),
            byte: 0,
            left: 0,
            zeros: 0,
        }
    }
    fn sps_prefix(&mut self) -> Option<(usize, u8)> {
        let profile = self.read(8)?;
        self.read(16)?;
        let sps = self.ue()? as usize;
        if sps > 31 {
            return None;
        }
        match profile {
            66 | 77 | 88 => (),
            100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135 => {
                if self.ue()? != 1 || self.ue()? != 0 || self.ue()? != 0 {
                    return None;
                }
                self.read(1)?;
                if self.read(1)? != 0 {
                    for list in 0..8 {
                        if self.read(1)? != 0 {
                            self.scaling_list(if list < 6 { 16 } else { 64 })?;
                        }
                    }
                }
            }
            _ => return None,
        }
        let count = self.ue()?;
        if count > 12 {
            return None;
        }
        Some((sps, count as u8 + 4))
    }
    fn read(&mut self, count: u8) -> Option<u32> {
        let mut value = 0;
        for _ in 0..count {
            if self.left == 0 {
                self.byte = *self.bytes.next()?;
                if self.zeros == 2 && self.byte == 3 {
                    self.zeros = 0;
                    self.byte = *self.bytes.next()?;
                }
                self.zeros = if self.byte == 0 {
                    (self.zeros + 1).min(2)
                } else {
                    0
                };
                self.left = 8;
            }
            self.left -= 1;
            value = (value << 1) | u32::from((self.byte >> self.left) & 1);
        }
        Some(value)
    }
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.read(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1_u32 << zeros) - 1 + self.read(zeros)?)
    }
    fn scaling_list(&mut self, length: usize) -> Option<()> {
        let mut last = 8;
        let mut next = 8;
        for _ in 0..length {
            if next != 0 {
                let code = i64::from(self.ue()?);
                let delta = if code % 2 == 0 {
                    -code / 2
                } else {
                    (code + 1) / 2
                };
                next = (last + delta).rem_euclid(256);
            }
            if next != 0 {
                last = next;
            }
        }
        Some(())
    }
}

#[cfg(test)]
mod gpu_decode_tests {
    use super::*;
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };
    fn area() -> Aperture {
        Aperture {
            x: 2,
            y: 4,
            size: PixelSize::new(640, 480),
        }
    }
    #[test]
    fn gpu_decode_requires_exact_host_generation_and_capabilities() {
        assert_eq!(
            gpu_decode_admission(None, 1, true, true),
            Err(MfGpuFallback::HostNotReady)
        );
        assert_eq!(
            gpu_decode_admission(Some(2), 1, true, true),
            Err(MfGpuFallback::DeviceMismatch)
        );
        assert_eq!(
            gpu_decode_admission(Some(1), 1, false, true),
            Err(MfGpuFallback::DeviceMismatch)
        );
        assert_eq!(
            gpu_decode_admission(Some(1), 1, true, false),
            Err(MfGpuFallback::UnsupportedDevice)
        );
        assert_eq!(gpu_decode_admission(Some(1), 1, true, true), Ok(()));
    }
    #[test]
    fn native_nv12_plane_copy_uses_exact_slice_and_plane_extent() {
        let plan = Nv12CopyPlan::new(PixelSize::new(672, 512), 8, 7, area(), area()).unwrap();
        assert_eq!(plan.slice, 7);
        assert_eq!(plan.plane(0), Some(((2, 4), PixelSize::new(640, 480))));
        assert_eq!(plan.plane(1), Some(((1, 2), PixelSize::new(320, 240))));
        assert_eq!(plan.plane(2), None);
        assert!(Nv12CopyPlan::new(plan.storage, 8, 8, area(), area()).is_err());
        assert!(Nv12CopyPlan::new(PixelSize::new(640, 480), 8, 7, area(), area()).is_err());
        assert!(Nv12CopyPlan::new(PixelSize::new(673, 512), 8, 7, area(), area()).is_err());
    }
    #[test]
    fn copy_lease_returns_sample_only_after_common_and_completion() {
        let mut lease = GpuLeaseState::reserved();
        lease.submit().unwrap();
        lease.picture_dropped = true;
        assert!(!lease.check_in_ready(true));
        assert!(!lease.release_sample());
        lease.completion = true;
        assert!(lease.check_in_ready(true));
        assert!(!lease.release_sample());
        lease.return_completed();
        assert!(lease.release_sample());
    }
    #[test]
    fn unsubmitted_superseded_picture_releases_without_gpu_access() {
        let mut lease = GpuLeaseState::reserved();
        lease.cancelled = true;
        lease.picture_dropped = true;
        assert!(!lease.check_in_ready(false));
        assert!(lease.check_in_ready(true));
        lease.return_completed();
        assert!(lease.release_sample());
        assert!(lease.submit().is_err());
    }
    #[test]
    fn copy_capacity_and_deadline_retire_without_recycling_uncertain_work() {
        assert_eq!(gpu_capacity(7, GPU_COPY_JOBS), Ok(()));
        assert_eq!(gpu_capacity(8, GPU_COPY_JOBS), Err(MfGpuFallback::PoolBusy));
        assert_eq!(
            gpu_capacity(8, GPU_SOURCE_LEASES),
            Err(MfGpuFallback::PoolBusy)
        );
        let mut lease = GpuLeaseState::reserved();
        lease.submit().unwrap();
        lease.quarantined = true;
        lease.picture_dropped = true;
        assert!(!lease.next_input_allowed());
        assert!(!lease.release_sample());
        assert!(!lease.check_in_ready(true));
        let now = Instant::now();
        assert_eq!(gpu_poll_deadline(now, [now]), None);
    }
    #[test]
    fn gpu_loss_refuses_old_generation_and_preserves_cpu_idr_fallback() {
        assert_eq!(
            gpu_decode_admission(None, 3, true, true),
            Err(MfGpuFallback::HostNotReady)
        );
        assert_eq!(
            gpu_decode_admission(Some(4), 3, true, true),
            Err(MfGpuFallback::DeviceMismatch)
        );
        let mut lease = GpuLeaseState::reserved();
        lease.quarantined = true;
        lease.picture_dropped = true;
        assert!(!lease.release_sample());
        assert!(!lease.next_input_allowed());
        assert_eq!(MfDecodePath::AwaitingCpuIdr.as_str(), "awaiting_cpu_idr");
        assert_eq!(MfDecodePath::CpuNv12.as_str(), "cpu_nv12");
    }
    #[test]
    fn gpu_ready_promotes_only_at_idr_once_per_generation() {
        assert_eq!(gpu_idr_binding(false, Some(2)), None);
        assert_eq!(gpu_idr_binding(true, None), None);
        assert_eq!(gpu_idr_binding(true, Some(2)), Some(2));
        assert_eq!(
            gpu_decode_admission(Some(1), 2, true, true),
            Err(MfGpuFallback::DeviceMismatch)
        );
        assert_eq!(gpu_decode_admission(Some(2), 2, true, true), Ok(()));
    }
    #[test]
    fn fresh_output_pairs_are_not_overwritten_after_source_release() {
        let mut serial = 0;
        let mut allocate = |_uv, size| {
            serial += 1;
            Arc::new((serial, size))
        };
        let first = allocate_gpu_planes(PixelSize::new(640, 480), &mut allocate);
        let held = first.clone();
        let second = allocate_gpu_planes(PixelSize::new(640, 480), &mut allocate);
        assert_ne!(first[0].0, second[0].0);
        assert_ne!(first[1].0, second[1].0);
        drop(first);
        assert_eq!(held[0].0, 1);
        assert_eq!(held[1].0, 2);
        assert_eq!(second[1].1, PixelSize::new(320, 240));
    }
    #[test]
    fn native_snapshot_unavailable_is_immediate_and_side_effect_free() {
        let mut out = Nv12 {
            y: vec![17, 18],
            ..Default::default()
        };
        let before = out.clone();
        assert!(matches!(
            native_cpu_cache(None, &mut out),
            Err(CodecError::Unavailable(_))
        ));
        assert_eq!(out.y, before.y);
        assert_eq!(out.size, before.size);
        // This helper receives no worker, callback, native resource or status sink.
    }
    #[test]
    fn output_reservation_prevents_next_input_before_publication() {
        let mut lease = GpuLeaseState::reserved();
        assert!(!lease.next_input_allowed());
        assert!(!lease.check_in_ready(false));
        lease.cancelled = true;
        assert!(!lease.next_input_allowed());
        lease.return_completed();
        assert!(lease.next_input_allowed());
        assert!(!lease.release_sample());
    }
    #[test]
    fn return_control_is_processed_before_waiting_decode() {
        let trace = std::cell::RefCell::new(Vec::new());
        let mut leases = vec![GpuLeaseState::reserved()];
        let blocked = gpu_drain_control(
            &mut leases,
            |lease| {
                trace.borrow_mut().push("return");
                lease.return_completed();
                false
            },
            |lease| {
                trace.borrow_mut().push("input");
                !lease.next_input_allowed()
            },
        );
        assert!(!blocked);
        assert_eq!(*trace.borrow(), vec!["return", "input"]);
    }
    #[test]
    fn sample_release_requires_last_picture_drop_and_safe_checkin() {
        let mut lease = GpuLeaseState::reserved();
        lease.submit().unwrap();
        lease.completion = true;
        lease.return_completed();
        assert!(lease.next_input_allowed());
        assert!(!lease.release_sample());
        lease.picture_dropped = true;
        assert!(lease.release_sample());
        let mut opposite = GpuLeaseState::reserved();
        opposite.picture_dropped = true;
        assert!(!opposite.release_sample());
        opposite.return_completed();
        assert!(opposite.release_sample());
    }
    #[test]
    fn copy_progress_wakes_without_redraw_and_stops_at_deadline() {
        let now = Instant::now();
        let end = now + GPU_BOUND;
        assert_eq!(gpu_poll_deadline(now, [end]), Some(now + GPU_POLL));
        assert_eq!(
            gpu_poll_deadline(end - Duration::from_millis(1), [end]),
            Some(end)
        );
        assert_eq!(gpu_poll_deadline(end, [end]), None);
        assert_eq!(
            gpu_poll_deadline(end + Duration::from_millis(1), [end]),
            None
        );
        assert_eq!(gpu_poll_deadline(now, []), None);
    }
    #[test]
    fn late_host_events_cannot_retire_or_revive_a_newer_generation() {
        assert_eq!(
            gpu_host_update(None, None, 1, true, false),
            GpuHostUpdate::Ready
        );
        assert_eq!(
            gpu_host_update(Some(2), Some(2), 1, false, false),
            GpuHostUpdate::RetireOther
        );
        assert_eq!(
            gpu_host_update(Some(2), Some(2), 1, true, false),
            GpuHostUpdate::Ignore
        );
        assert_eq!(
            gpu_host_update(Some(2), Some(2), 2, true, false),
            GpuHostUpdate::Ignore
        );
        assert_eq!(
            gpu_host_update(Some(2), Some(2), 2, false, false),
            GpuHostUpdate::RetireCurrent
        );
        assert_eq!(
            gpu_host_update(Some(2), None, 2, true, true),
            GpuHostUpdate::Ignore
        );
        assert_eq!(
            gpu_host_update(Some(2), None, 3, true, false),
            GpuHostUpdate::Ready
        );
    }
    #[test]
    fn retirement_preserves_return_proof_and_releases_once_after_final_drop() {
        let mut returned = GpuLeaseState::reserved();
        returned.submit().unwrap();
        returned.completion = true;
        returned.return_completed();
        returned.quarantine();
        assert!(returned.returned);
        assert!(!returned.quarantined);
        assert!(!returned.release_sample());
        let mut leases = vec![returned];
        let mut released = 0;
        leases[0].picture_dropped = true;
        for _ in 0..2 {
            assert!(!gpu_drain_control(
                &mut leases,
                |lease| {
                    if lease.release_sample() {
                        released += 1;
                        true
                    } else {
                        false
                    }
                },
                |lease| !lease.next_input_allowed()
            ));
        }
        assert_eq!(released, 1);
        assert!(leases.is_empty());
    }
    #[test]
    fn retired_lease_releases_after_late_return_then_final_drop() {
        let mut lease = GpuLeaseState::reserved();
        lease.submit().unwrap();
        lease.quarantine();
        lease.completion = true;
        assert!(lease.check_in_ready(true));
        lease.return_completed();
        assert!(!lease.release_sample());
        lease.picture_dropped = true;
        assert!(lease.release_sample());
        assert!(!lease.quarantined);
    }
    #[test]
    fn retired_unreturned_lease_stays_counted_until_proven_return_and_drop() {
        let mut lease = GpuLeaseState::reserved();
        lease.submit().unwrap();
        lease.quarantine();
        lease.picture_dropped = true;
        let mut leases = vec![lease];
        assert!(gpu_drain_control(
            &mut leases,
            |lease| lease.release_sample(),
            |lease| !lease.next_input_allowed()
        ));
        assert_eq!(leases.len(), 1);
        assert!(gpu_capacity(leases.len(), 1).is_err());
        assert!(!leases[0].check_in_ready(true));
        // Only observed completion plus successful native return can settle quarantine.
        leases[0].completion = true;
        assert!(leases[0].check_in_ready(true));
        leases[0].return_completed();
        assert!(!gpu_drain_control(
            &mut leases,
            |lease| lease.release_sample(),
            |lease| !lease.next_input_allowed()
        ));
        assert!(leases.is_empty());
    }
    #[test]
    fn picture_retirement_cannot_be_overwritten_by_late_import_success() {
        let outcome = (
            MfDecodePath::AwaitingCpuIdr,
            Some(MfGpuFallback::DeviceLost),
        );
        let failure = Some(outcome);
        assert_eq!(
            gpu_path_update(failure, (MfDecodePath::DxgiGpuCopy, None)),
            failure
        );
        assert_eq!(
            gpu_path_update(None, (MfDecodePath::DxgiGpuCopy, None)),
            Some((MfDecodePath::DxgiGpuCopy, None))
        );
        assert_eq!(
            gpu_path_update(Some((MfDecodePath::DxgiGpuCopy, None)), outcome),
            failure
        );
    }
    #[test]
    fn malformed_empty_nals_never_classify_as_idr() {
        for bytes in [&[][..], &[0, 0, 1][..], &[0, 0, 1, 0, 0, 1][..]] {
            assert!(!has_nal(bytes, 5));
            assert!(!has_nal(bytes, 7));
        }
        assert!(has_nal(&[0, 0, 1, 0x65, 0, 0, 1], 5));
        // The decoder's existing admission refuses malformed units, without a panic.
        for bytes in [&[0, 0, 1][..], &[0, 0, 1, 0, 0, 1][..]] {
            assert!(References::default().check(bytes).is_err());
        }
    }
    #[test]
    fn unsubmitted_checkout_waits_for_ready_before_checkin() {
        let mut lease = GpuLeaseState::reserved();
        lease.picture_dropped = true;
        lease.cancelled = true;
        assert!(!lease.check_in_ready(false));
        assert!(!lease.release_sample());
        assert!(lease.check_in_ready(true));
        lease.return_completed();
        assert!(lease.release_sample());
    }
}
