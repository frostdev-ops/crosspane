//! OS-free realtime H.264 parameter, pixel and event contracts.

use crosspane_media::{
    codec::CodecError,
    picture::{Nv12, YuvColour},
};
use crosspane_types::geom::PixelSize;

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
