//! Pure clipboard metadata, text and bounded-wait decisions. Never formats content.
use crosspane_platform::{ClipKinds, PlatformError};
use crosspane_types::ClipKind;
use zeroize::Zeroizing;

pub const TEXT_CAP: usize = 1024 * 1024;
pub const IMAGE_CAP: usize = 16 * 1024 * 1024;
pub const DIB_WORK_CAP: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default)]
pub struct Formats {
    pub text: bool,
    pub png: bool,
    pub dib_v5: bool,
    pub dib: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Text,
    Png,
    DibV5,
    Dib,
}
impl Formats {
    pub fn kinds(self) -> ClipKinds {
        ClipKinds {
            text: self.text,
            image: self.png || self.dib_v5 || self.dib,
        }
    }
    pub fn select(self, kind: ClipKind, own: bool) -> Result<Format, PlatformError> {
        if own {
            return Err(PlatformError::NotFound);
        }
        match kind {
            ClipKind::Text if self.text => Ok(Format::Text),
            ClipKind::Image if self.png => Ok(Format::Png),
            ClipKind::Image if self.dib_v5 => Ok(Format::DibV5),
            ClipKind::Image if self.dib => Ok(Format::Dib),
            _ => Err(PlatformError::NotFound),
        }
    }
}
pub fn text(native: &[u8], max_bytes: usize) -> Result<Vec<u8>, PlatformError> {
    let limit = max_bytes.min(TEXT_CAP);
    // CRLF is the only shrinking conversion: two native units become one UTF-8 byte.
    let scanned = 2 * limit + 2;
    let end = native
        .as_chunks::<2>()
        .0
        .iter()
        .take(scanned)
        .position(|unit| unit == &[0, 0])
        .ok_or(if native.len() / 2 > scanned {
            PlatformError::TooLarge
        } else {
            PlatformError::NotFound
        })?;
    let units = native[..end * 2]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|u| u16::from_le_bytes([u[0], u[1]]));
    let mut chars = char::decode_utf16(units).peekable();
    let mut output = Zeroizing::new(Vec::new());
    while let Some(ch) = chars.next() {
        let mut ch = ch.map_err(|_| PlatformError::NotFound)?;
        if ch == '\r' && matches!(chars.peek(), Some(Ok('\n'))) {
            chars.next();
            ch = '\n';
        }
        let mut buf = [0; 4];
        let encoded = ch.encode_utf8(&mut buf).as_bytes();
        if encoded.len() > limit.saturating_sub(output.len()) {
            return Err(PlatformError::TooLarge);
        }
        output
            .try_reserve(encoded.len())
            .map_err(|_| allocation())?;
        output.extend_from_slice(encoded);
    }
    if output.is_empty() {
        Err(PlatformError::NotFound)
    } else {
        Ok(std::mem::take(&mut *output))
    }
}
pub fn png(native: &[u8], max_bytes: usize) -> Result<Vec<u8>, PlatformError> {
    if native.is_empty() {
        return Err(PlatformError::NotFound);
    }
    if native.len() > max_bytes.min(IMAGE_CAP) {
        return Err(PlatformError::TooLarge);
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(native.len())
        .map_err(|_| allocation())?;
    output.extend_from_slice(native);
    Ok(output)
}
fn layout() -> PlatformError {
    PlatformError::Backend("unsupported clipboard image layout".into())
}
fn allocation() -> PlatformError {
    PlatformError::Backend("clipboard allocation failed".into())
}
fn field(raw: &[u8], at: usize) -> Result<u32, PlatformError> {
    let b = raw.get(at..at + 4).ok_or_else(layout)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}
fn short(raw: &[u8], at: usize) -> Result<u16, PlatformError> {
    let b = raw.get(at..at + 2).ok_or_else(layout)?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}
fn component(pixel: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 255;
    }
    let shifted = u64::from(mask >> mask.trailing_zeros());
    ((u64::from((pixel & mask) >> mask.trailing_zeros()) * 255 + shifted / 2) / shifted) as u8
}
/// Lead-approved decoded RGBA work bound, independent of the compressed wire/output cap.
/// Only uncompressed RGB/bitfields and sRGB/default color are accepted; no profile/file API.
pub fn dib_png(raw: &[u8], max_bytes: usize) -> Result<Vec<u8>, PlatformError> {
    let header = field(raw, 0)? as usize;
    if ![40, 52, 56, 108, 124].contains(&header) || raw.len() < header {
        return Err(layout());
    }
    let width = field(raw, 4)? as i32;
    let signed_height = field(raw, 8)? as i32;
    let bits = short(raw, 14)?;
    let compression = field(raw, 16)?;
    if width <= 0
        || signed_height == 0
        || short(raw, 12)? != 1
        || ![1, 4, 8, 16, 24, 32].contains(&bits)
        || ![0, 3].contains(&compression)
        || (compression == 3 && ![16, 32].contains(&bits))
    {
        return Err(layout());
    }
    if header >= 108 && ![0x7352_4742, 0x5769_6e20].contains(&field(raw, 56)?) {
        return Err(layout());
    }
    if header == 124 && (field(raw, 112)? != 0 || field(raw, 116)? != 0) {
        return Err(layout());
    }
    let width = width as usize;
    let height = signed_height.unsigned_abs() as usize;
    let work = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(4))
        .ok_or(PlatformError::TooLarge)?;
    if work > DIB_WORK_CAP {
        return Err(PlatformError::TooLarge);
    }
    let stride = width
        .checked_mul(usize::from(bits))
        .and_then(|n| n.checked_add(31))
        .and_then(|n| (n / 32).checked_mul(4))
        .ok_or(PlatformError::TooLarge)?;
    let masks = if compression == 3 {
        [
            field(raw, 40)?,
            field(raw, 44)?,
            field(raw, 48)?,
            if header >= 56 { field(raw, 52)? } else { 0 },
        ]
    } else if bits == 16 {
        [0x7c00, 0x03e0, 0x001f, 0]
    } else {
        [0xff0000, 0xff00, 0xff, 0]
    };
    if bits >= 16 {
        let mut used = 0;
        for (i, mask) in masks.into_iter().enumerate() {
            if mask == 0 {
                if i < 3 {
                    return Err(layout());
                } else {
                    continue;
                }
            }
            let shifted = mask >> mask.trailing_zeros();
            if shifted & shifted.wrapping_add(1) != 0
                || used & mask != 0
                || (bits < 32 && mask >> bits != 0)
            {
                return Err(layout());
            }
            used |= mask;
        }
    }
    let palette_count = field(raw, 32)? as usize;
    let palette_count = if bits <= 8 && palette_count == 0 {
        1usize << bits
    } else {
        palette_count
    };
    if bits <= 8 && palette_count > 1usize << bits {
        return Err(layout());
    }
    let palette_start = header
        + if header == 40 && compression == 3 {
            12
        } else {
            0
        };
    let pixels_start = palette_count
        .checked_mul(4)
        .and_then(|n| n.checked_add(palette_start))
        .ok_or_else(layout)?;
    let end = stride
        .checked_mul(height)
        .and_then(|n| n.checked_add(pixels_start))
        .ok_or_else(layout)?;
    if end > raw.len() {
        return Err(layout());
    }
    let mut pixels = Zeroizing::new(Vec::new());
    pixels.try_reserve_exact(work).map_err(|_| allocation())?;
    pixels.resize(work, 0);
    for y in 0..height {
        let source_y = if signed_height > 0 { height - 1 - y } else { y };
        let row = &raw[pixels_start + source_y * stride..pixels_start + (source_y + 1) * stride];
        for x in 0..width {
            let rgba = match bits {
                1 | 4 | 8 => {
                    let index = match bits {
                        1 => (row[x / 8] >> (7 - x % 8)) & 1,
                        4 => (row[x / 2] >> (if x % 2 == 0 { 4 } else { 0 })) & 15,
                        _ => row[x],
                    } as usize;
                    if index >= palette_count {
                        return Err(layout());
                    }
                    let p = palette_start + index * 4;
                    [raw[p + 2], raw[p + 1], raw[p], 255]
                }
                24 => [row[x * 3 + 2], row[x * 3 + 1], row[x * 3], 255],
                _ => {
                    let p = if bits == 16 {
                        u32::from(u16::from_le_bytes([row[x * 2], row[x * 2 + 1]]))
                    } else {
                        u32::from_le_bytes([
                            row[x * 4],
                            row[x * 4 + 1],
                            row[x * 4 + 2],
                            row[x * 4 + 3],
                        ])
                    };
                    masks.map(|m| component(p, m))
                }
            };
            pixels[(y * width + x) * 4..(y * width + x + 1) * 4].copy_from_slice(&rgba);
        }
    }
    let mut output = Capped {
        bytes: Zeroizing::new(Vec::new()),
        limit: max_bytes.min(IMAGE_CAP),
        exceeded: false,
    };
    let result = (|| {
        let mut encoder = ::png::Encoder::new(&mut output, width as u32, height as u32);
        encoder.set_color(::png::ColorType::Rgba);
        encoder.set_depth(::png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&pixels)?;
        writer.finish()
    })();
    if output.exceeded {
        return Err(PlatformError::TooLarge);
    }
    result.map_err(|_| PlatformError::Backend("encode clipboard image failed".into()))?;
    Ok(std::mem::take(&mut *output.bytes))
}
struct Capped {
    bytes: Zeroizing<Vec<u8>>,
    limit: usize,
    exceeded: bool,
}
impl std::io::Write for Capped {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(std::io::Error::other("clipboard image output limit"));
        }
        self.bytes
            .try_reserve(bytes.len())
            .map_err(|_| std::io::Error::other("clipboard image allocation"))?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub fn admit(
    open: bool,
    initial_epoch: u64,
    current_epoch: u64,
    expired: bool,
) -> Result<(), PlatformError> {
    if !open || initial_epoch != current_epoch {
        Err(PlatformError::Locked)
    } else if expired {
        Err(PlatformError::Timeout)
    } else {
        Ok(())
    }
}
pub fn retry_delay(elapsed_ms: u64) -> Result<u64, PlatformError> {
    if elapsed_ms >= 250 {
        Err(PlatformError::Timeout)
    } else {
        Ok(5.min(250 - elapsed_ms))
    }
}
pub fn coherent(before: (u32, usize), after: (u32, usize)) -> bool {
    before.0 != 0 && before == after
}

#[derive(Clone, Copy, Debug)]
pub struct Snapshot {
    pub sequence: u32,
    pub owner: usize,
    pub kinds: ClipKinds,
}
#[derive(Default, Debug)]
pub struct Watch {
    delivered: Option<u32>,
}
impl Watch {
    pub fn observe(&mut self, snapshot: Snapshot, own: usize, open: bool) -> Option<ClipKinds> {
        if !open || self.delivered == Some(snapshot.sequence) {
            return None;
        }
        self.delivered = Some(snapshot.sequence);
        if own != 0 && snapshot.owner == own {
            None
        } else {
            Some(snapshot.kinds)
        }
    }
}
