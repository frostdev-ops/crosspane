//! Pixel layout conversion for CPU frames (WP-G2.2). Pure functions.
//!
//! The ScreenCast PipeWire stream negotiates one of the 32-bit packed layouts in `PixelFormat`
//! and delivers CPU buffers. `to_bgra` turns one buffer into tight BGRA with the alpha byte forced
//! to `0xFF`; `crop_bgra` cuts a rectangle out of tight BGRA. Sizes and strides are validated
//! before the output is allocated, and source rows are taken with `get`, so no input can make
//! these functions panic.

use std::sync::Arc;

use crosspane_types::geom::PixelRect;

/// 32-bit packed layouts, named by byte order in memory (`Bgrx` is bytes B, G, R, x).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PixelFormat {
    Bgrx, // bytes B G R x
    Bgra, // bytes B G R A
    Rgbx, // bytes R G B x
    Rgba, // bytes R G B A
    Xrgb, // bytes x R G B
}

/// Why a conversion was refused. Checks run in a fixed order, before anything is allocated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PixelError {
    ZeroSize,    // width == 0 or height == 0
    TooLarge,    // width * height > MAX_PIXELS
    BadStride,   // stride < width * 4 (or the product overflows)
    ShortBuffer, // src shorter than (height - 1) * stride + width * 4 (checked arithmetic)
    BadRect,     // crop rectangle empty, negative, or not inside the image
    Alloc,       // no output buffer could be had
}

/// Largest image accepted: 2^26 pixels (256 MiB of BGRA).
pub(super) const MAX_PIXELS: u64 = 1 << 26;

/// Convert a whole image to tightly packed BGRA (rows of width*4 bytes, top row first, i.e. the
/// source row order). `src` starts at the first row; rows are `stride` bytes apart; the last row
/// need not carry stride padding (src.len() >= (height-1)*stride + width*4 is enough). The output
/// alpha byte is ALWAYS 0xFF (screen content is opaque; an `x` byte or a producer's alpha must not
/// leak into pixel hashing).
///
/// The result is a shared buffer filled in place: a frame that shows the whole image takes it
/// without a copy.
pub(super) fn to_bgra_shared(
    format: PixelFormat,
    src: &[u8],
    stride: usize,
    width: u32,
    height: u32,
) -> Result<Arc<[u8]>, PixelError> {
    check_dims(width, height)?;
    let row = row_bytes(width)?;
    if stride < row {
        return Err(PixelError::BadStride);
    }
    if src.len() < required_len(stride, row, height)? {
        return Err(PixelError::ShortBuffer);
    }
    let out_len = tight_len(width, height)?;
    // One allocation of the final size (the iterator knows its length), filled below.
    let mut out: Arc<[u8]> = std::iter::repeat_n(0_u8, out_len).collect();
    let dst = Arc::get_mut(&mut out).ok_or(PixelError::Alloc)?;
    for (y, dst_row) in dst.chunks_exact_mut(row).enumerate() {
        // `y < height` and `(height - 1) * stride + row` was checked above, so this cannot
        // overflow, and the `get` below cannot miss.
        let start = y * stride;
        let src_row = src.get(start..start + row).ok_or(PixelError::ShortBuffer)?;
        convert_row(format, src_row, dst_row);
    }
    Ok(out)
}

/// Copy `rect` (device pixels, must satisfy 0 <= min < max <= width/height) out of a TIGHT BGRA
/// image (rows of width*4 bytes) into a new tight BGRA buffer of rect-size. `src.len()` must be
/// at least width*height*4 (else ShortBuffer). Alpha bytes are copied unchanged.
pub(super) fn crop_bgra(
    src: &[u8],
    width: u32,
    height: u32,
    rect: PixelRect,
) -> Result<Vec<u8>, PixelError> {
    check_dims(width, height)?;
    if src.len() < tight_len(width, height)? {
        return Err(PixelError::ShortBuffer);
    }
    let (x0, y0, x1, y1) = crop_bounds(width, height, rect)?;
    let crop_row = row_bytes(x1 - x0)?;
    let out_len = tight_len(x1 - x0, y1 - y0)?;
    let mut out = Vec::new();
    out.try_reserve_exact(out_len)
        .map_err(|_| PixelError::Alloc)?;
    for y in y0..y1 {
        let start = tight_offset(width, x0, y)?;
        let src_row = src
            .get(start..start + crop_row)
            .ok_or(PixelError::ShortBuffer)?;
        out.extend_from_slice(src_row);
    }
    Ok(out)
}

/// Rejects empty images, then images over `MAX_PIXELS`.
fn check_dims(width: u32, height: u32) -> Result<(), PixelError> {
    if width == 0 || height == 0 {
        return Err(PixelError::ZeroSize);
    }
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(PixelError::TooLarge);
    }
    Ok(())
}

/// `usize` from a `u64` size. This fails only where `usize` is narrower than the value.
fn to_usize(value: u64) -> Result<usize, PixelError> {
    usize::try_from(value).map_err(|_| PixelError::TooLarge)
}

/// Bytes in one row of `width` 32-bit pixels.
fn row_bytes(width: u32) -> Result<usize, PixelError> {
    to_usize(u64::from(width) * 4)
}

/// Bytes a `height`-row image needs in the source: `(height - 1) * stride + row`. A product or
/// sum that overflows `usize` cannot describe any buffer, so it is reported as a bad stride.
fn required_len(stride: usize, row: usize, height: u32) -> Result<usize, PixelError> {
    let gaps = to_usize(u64::from(height.saturating_sub(1)))?;
    gaps.checked_mul(stride)
        .and_then(|bytes| bytes.checked_add(row))
        .ok_or(PixelError::BadStride)
}

/// Bytes of a tight BGRA image of `width` x `height`. Callers check `check_dims` first.
fn tight_len(width: u32, height: u32) -> Result<usize, PixelError> {
    let bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(PixelError::TooLarge)?;
    to_usize(bytes)
}

/// Byte offset of pixel `(x, y)` in a tight image `width` pixels wide.
fn tight_offset(width: u32, x: u32, y: u32) -> Result<usize, PixelError> {
    let pixel = u64::from(y)
        .checked_mul(u64::from(width))
        .and_then(|p| p.checked_add(u64::from(x)))
        .and_then(|p| p.checked_mul(4))
        .ok_or(PixelError::TooLarge)?;
    to_usize(pixel)
}

/// Checks `rect` against the image and returns its `(x0, y0, x1, y1)` bounds in pixels. Negative
/// coordinates have no `u32` form, so they are refused here.
fn crop_bounds(
    width: u32,
    height: u32,
    rect: PixelRect,
) -> Result<(u32, u32, u32, u32), PixelError> {
    let x0 = u32::try_from(rect.min.x).map_err(|_| PixelError::BadRect)?;
    let y0 = u32::try_from(rect.min.y).map_err(|_| PixelError::BadRect)?;
    let x1 = u32::try_from(rect.max.x).map_err(|_| PixelError::BadRect)?;
    let y1 = u32::try_from(rect.max.y).map_err(|_| PixelError::BadRect)?;
    if x0 >= x1 || y0 >= y1 || x1 > width || y1 > height {
        return Err(PixelError::BadRect);
    }
    Ok((x0, y0, x1, y1))
}

/// Converts one row. `src` and `dst` are each exactly one row (`width * 4` bytes, a whole number
/// of pixels), which the callers guarantee, so the copies below cannot panic.
fn convert_row(format: PixelFormat, src: &[u8], dst: &mut [u8]) {
    match format {
        PixelFormat::Bgrx | PixelFormat::Bgra => {
            dst.copy_from_slice(src);
            set_opaque(dst);
        }
        PixelFormat::Rgbx | PixelFormat::Rgba => {
            let (out_px, _) = dst.as_chunks_mut::<4>();
            let (in_px, _) = src.as_chunks::<4>();
            for (o, s) in out_px.iter_mut().zip(in_px) {
                *o = [s[2], s[1], s[0], 0xFF];
            }
        }
        PixelFormat::Xrgb => {
            let (out_px, _) = dst.as_chunks_mut::<4>();
            let (in_px, _) = src.as_chunks::<4>();
            for (o, s) in out_px.iter_mut().zip(in_px) {
                *o = [s[3], s[2], s[1], 0xFF];
            }
        }
    }
}

/// Sets the alpha byte (index 3 of every pixel) of a BGRA row to `0xFF`.
fn set_opaque(bgra: &mut [u8]) {
    let (px, _) = bgra.as_chunks_mut::<4>();
    for p in px {
        p[3] = 0xFF;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosspane_types::geom::euclid::point2;

    /// `to_bgra_shared` as a plain vector, to compare with.
    fn to_bgra(
        format: PixelFormat,
        src: &[u8],
        stride: usize,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, PixelError> {
        to_bgra_shared(format, src, stride, width, height).map(|shared| shared.to_vec())
    }

    /// Deterministic linear congruential generator (Knuth's MMIX constants). No randomness crate.
    struct Lcg(u64);

    impl Lcg {
        fn next_byte(&mut self) -> u8 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 56) as u8
        }
    }

    fn rect(x0: i32, y0: i32, x1: i32, y1: i32) -> PixelRect {
        PixelRect::new(point2(x0, y0), point2(x1, y1))
    }

    /// Memory bytes of one pixel with colour `(b, g, r)` in `format`; `x` is the spare byte.
    fn encode(format: PixelFormat, b: u8, g: u8, r: u8, x: u8) -> [u8; 4] {
        match format {
            PixelFormat::Bgrx | PixelFormat::Bgra => [b, g, r, x],
            PixelFormat::Rgbx | PixelFormat::Rgba => [r, g, b, x],
            PixelFormat::Xrgb => [x, r, g, b],
        }
    }

    /// A 3x2 image with stride 16 (4 pad bytes of 0xEE per row) and its expected tight BGRA.
    /// Pixel `i` has colour `(0x20 + i, 0x40 + i, 0x60 + i)` in B, G, R order.
    fn padded_3x2(format: PixelFormat, spare: u8) -> (Vec<u8>, Vec<u8>) {
        let mut src = Vec::new();
        let mut expected = Vec::new();
        for row in 0..2u8 {
            for col in 0..3u8 {
                let i = row * 3 + col;
                let (b, g, r) = (0x20 + i, 0x40 + i, 0x60 + i);
                src.extend_from_slice(&encode(format, b, g, r, spare));
                expected.extend_from_slice(&[b, g, r, 0xFF]);
            }
            src.extend_from_slice(&[0xEE; 4]);
        }
        (src, expected)
    }

    /// A 5x4 tight BGRA image whose byte `i` is `i`: pixel `p` (= y * 5 + x) holds bytes 4p..4p+4.
    fn ramp_5x4() -> Vec<u8> {
        (0..80u8).collect()
    }

    #[test]
    fn each_format_converts_a_padded_3x2_image() {
        let formats = [
            PixelFormat::Bgrx,
            PixelFormat::Bgra,
            PixelFormat::Rgbx,
            PixelFormat::Rgba,
            PixelFormat::Xrgb,
        ];
        for format in formats {
            // The spare byte is 0x00 or 0x7F; the output alpha must be 0xFF either way.
            for spare in [0x00, 0x7F] {
                let (src, expected) = padded_3x2(format, spare);
                assert_eq!(expected.len(), 24);
                assert_eq!(
                    to_bgra(format, &src, 16, 3, 2).unwrap(),
                    expected,
                    "{format:?} spare {spare:#x}"
                );
            }
        }
    }

    #[test]
    fn tight_and_unpadded_last_row_are_accepted() {
        let (padded, expected) = padded_3x2(PixelFormat::Rgba, 0x7F);
        // Stride == width * 4: the rows sit back to back.
        let tight: Vec<u8> = padded
            .chunks(16)
            .flat_map(|row| row[..12].to_vec())
            .collect();
        assert_eq!(tight.len(), 24);
        assert_eq!(
            to_bgra(PixelFormat::Rgba, &tight, 12, 3, 2).unwrap(),
            expected
        );
        // Stride 16, but the last row carries no padding: 28 bytes are enough.
        assert_eq!(
            to_bgra(PixelFormat::Rgba, &padded[..28], 16, 3, 2).unwrap(),
            expected
        );
        // One byte short is refused, for both strides.
        assert_eq!(
            to_bgra(PixelFormat::Rgba, &padded[..27], 16, 3, 2),
            Err(PixelError::ShortBuffer)
        );
        assert_eq!(
            to_bgra(PixelFormat::Rgba, &tight[..23], 12, 3, 2),
            Err(PixelError::ShortBuffer)
        );
    }

    #[test]
    fn bad_sizes_and_strides_are_refused() {
        let src = [0u8; 64];
        assert_eq!(
            to_bgra(PixelFormat::Bgra, &src, 4, 0, 0),
            Err(PixelError::ZeroSize)
        );
        assert_eq!(
            to_bgra(PixelFormat::Bgra, &src, 4, 0, 1),
            Err(PixelError::ZeroSize)
        );
        assert_eq!(
            to_bgra(PixelFormat::Bgra, &src, 4, 1, 0),
            Err(PixelError::ZeroSize)
        );
        // 16385 * 16385 > 2^26 pixels: refused before the (empty) source is looked at.
        assert_eq!(
            to_bgra(PixelFormat::Bgra, &[], 16385 * 4, 16385, 16385),
            Err(PixelError::TooLarge)
        );
        // Exactly 2^26 pixels is accepted; the empty source is then short. Nothing is allocated.
        assert_eq!(
            to_bgra(PixelFormat::Bgra, &[], 65536 * 4, 65536, 1024),
            Err(PixelError::ShortBuffer)
        );
        // Stride one byte short of a row.
        assert_eq!(
            to_bgra(PixelFormat::Bgra, &src, 3 * 4 - 1, 3, 2),
            Err(PixelError::BadStride)
        );
        // Stride arithmetic near usize::MAX must not panic.
        assert_eq!(
            to_bgra(PixelFormat::Bgra, &src, usize::MAX, 1, 2),
            Err(PixelError::BadStride)
        );
        assert_eq!(
            to_bgra(PixelFormat::Bgra, &src, usize::MAX / 2 + 1, 1, 3),
            Err(PixelError::BadStride)
        );
    }

    #[test]
    fn crop_full_image_is_identical() {
        let img = ramp_5x4();
        assert_eq!(crop_bgra(&img, 5, 4, rect(0, 0, 5, 4)).unwrap(), img);
    }

    #[test]
    fn crop_interior_rectangle_matches_bytes() {
        let img = ramp_5x4();
        // 2x2 at (1, 1): pixels 6, 7 (row 1) and 11, 12 (row 2).
        let expected: Vec<u8> = (24u8..32).chain(44..52).collect();
        assert_eq!(crop_bgra(&img, 5, 4, rect(1, 1, 3, 3)).unwrap(), expected);
    }

    #[test]
    fn crop_bottom_right_pixel() {
        let img = ramp_5x4();
        // Pixel 19 is at (4, 3).
        assert_eq!(
            crop_bgra(&img, 5, 4, rect(4, 3, 5, 4)).unwrap(),
            vec![76, 77, 78, 79]
        );
    }

    #[test]
    fn crop_bad_rectangles_are_refused() {
        let img = ramp_5x4();
        let bad = [
            rect(-1, 0, 2, 2),  // negative min x
            rect(0, -1, 2, 2),  // negative min y
            rect(0, 0, -1, -1), // negative max
            rect(0, 0, 6, 4),   // max x beyond the width
            rect(0, 0, 5, 5),   // max y beyond the height
            rect(2, 1, 2, 3),   // empty (min == max in x)
            rect(0, 3, 5, 3),   // empty (min == max in y)
            rect(3, 1, 2, 3),   // inverted
        ];
        for r in bad {
            assert_eq!(crop_bgra(&img, 5, 4, r), Err(PixelError::BadRect));
        }
    }

    #[test]
    fn crop_size_and_source_errors_come_first() {
        let img = ramp_5x4();
        // A short source is reported before a bad rectangle.
        assert_eq!(
            crop_bgra(&img[..79], 5, 4, rect(-1, 0, 1, 1)),
            Err(PixelError::ShortBuffer)
        );
        assert_eq!(
            crop_bgra(&[], 5, 4, rect(0, 0, 1, 1)),
            Err(PixelError::ShortBuffer)
        );
        assert_eq!(
            crop_bgra(&[], 0, 4, rect(0, 0, 1, 1)),
            Err(PixelError::ZeroSize)
        );
        assert_eq!(
            crop_bgra(&[], 16385, 16385, rect(0, 0, 1, 1)),
            Err(PixelError::TooLarge)
        );
    }

    /// The naive per-pixel reference: each pixel's four bytes are mapped through the format's
    /// byte order and alpha is forced to 0xFF.
    fn reference(
        format: PixelFormat,
        src: &[u8],
        stride: usize,
        width: usize,
        height: usize,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let p = &src[y * stride + x * 4..][..4];
                let (b, g, r) = match format {
                    PixelFormat::Bgrx | PixelFormat::Bgra => (p[0], p[1], p[2]),
                    PixelFormat::Rgbx | PixelFormat::Rgba => (p[2], p[1], p[0]),
                    PixelFormat::Xrgb => (p[3], p[2], p[1]),
                };
                out.extend_from_slice(&[b, g, r, 0xFF]);
            }
        }
        out
    }

    #[test]
    fn to_bgra_matches_the_naive_reference() {
        let mut lcg = Lcg(0x5EED);
        let formats = [
            PixelFormat::Bgrx,
            PixelFormat::Bgra,
            PixelFormat::Rgbx,
            PixelFormat::Rgba,
            PixelFormat::Xrgb,
        ];
        for (width, height) in [(1u32, 1u32), (3, 2), (7, 5), (16, 3)] {
            let row = width as usize * 4;
            for stride in [row, row + 4, row + 13] {
                for extra in [0usize, 3] {
                    for format in formats {
                        // Exact length for the last row, plus `extra` trailing bytes.
                        let len = (height as usize - 1) * stride + row + extra;
                        let src: Vec<u8> = (0..len).map(|_| lcg.next_byte()).collect();
                        let got = to_bgra(format, &src, stride, width, height).unwrap();
                        let want = reference(format, &src, stride, width as usize, height as usize);
                        assert_eq!(
                            got, want,
                            "{format:?} {width}x{height} stride {stride} extra {extra}"
                        );
                    }
                }
            }
        }
    }
}
