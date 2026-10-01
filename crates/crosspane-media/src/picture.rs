//! Decoded video pictures (03 §6 video layer): 4:2:0 NV12 planes and their colour description.
//!
//! Decoders hand the renderer NV12 instead of BGRA, so the YUV → RGB conversion runs in the
//! renderer's shader and 1.5 bytes per pixel cross to the GPU instead of 4. [`YuvToRgb`] is the one
//! definition of that conversion: the shader gets its numbers as a uniform, and
//! [`nv12_to_bgra`] is the CPU reference the shader is tested against (and what snapshots use).

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use crosspane_types::geom::PixelSize;

use crate::codec::CodecError;

/// The YUV → RGB matrix of a picture (ITU-T H.273 `MatrixCoefficients`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum YuvMatrix {
    /// BT.601 (H.273 values 5 and 6).
    Bt601,
    /// BT.709 (H.273 value 1); also what a picture without a colour description is taken to be.
    #[default]
    Bt709,
}

/// How a picture's 8-bit samples map to RGB.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct YuvColour {
    pub matrix: YuvMatrix,
    /// Full range (luma and chroma use 0–255) instead of limited range (luma 16–235, chroma
    /// 16–240).
    pub full_range: bool,
}

/// The affine map from 8-bit (Y, Cb, Cr) samples to R′G′B′ in 0–1 (sRGB-encoded, like the
/// captured BGRA): `rgb = clamp(rows · (sample / 255 − offset), 0, 1)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct YuvToRgb {
    /// Subtracted from (Y, Cb, Cr) / 255.
    pub offset: [f32; 3],
    /// The R, G and B rows, each a weight for (Y, Cb, Cr) after the offset.
    pub rows: [[f32; 3]; 3],
}

impl YuvColour {
    /// The conversion for this colour description (H.273 §8.6, 8-bit).
    pub fn to_rgb(self) -> YuvToRgb {
        let (kr, kb) = match self.matrix {
            YuvMatrix::Bt601 => (0.299_f64, 0.114_f64),
            YuvMatrix::Bt709 => (0.2126, 0.0722),
        };
        let kg = 1.0 - kr - kb;
        // Limited range stretches 219 luma and 224 chroma codes over the full scale.
        let (luma, chroma, black) = if self.full_range {
            (1.0, 1.0, 0.0)
        } else {
            (255.0 / 219.0, 255.0 / 224.0, 16.0 / 255.0)
        };
        let rows = [
            [luma, 0.0, 2.0 * (1.0 - kr) * chroma],
            [
                luma,
                -2.0 * kb * (1.0 - kb) / kg * chroma,
                -2.0 * kr * (1.0 - kr) / kg * chroma,
            ],
            [luma, 2.0 * (1.0 - kb) * chroma, 0.0],
        ];
        YuvToRgb {
            offset: [black as f32, 128.0 / 255.0, 128.0 / 255.0],
            rows: rows.map(|row| row.map(|weight| weight as f32)),
        }
    }
}

/// A decoded 4:2:0 picture in NV12 layout.
///
/// - `size` is the coded size: both dimensions even and nonzero. It can exceed the frame's real
///   size by the encoder's padding (see [`crate::codec::VideoEncoder::encode`]).
/// - `y`: `size.height` rows of `y_stride` bytes; the first `size.width` bytes of each row are
///   luma samples.
/// - `uv`: `size.height / 2` rows of `uv_stride` bytes; the first `size.width` bytes of each row
///   are `size.width / 2` interleaved (Cb, Cr) pairs. The pair at (x / 2, y / 2) belongs to the
///   2×2 block of luma samples whose top-left is at (x, y).
/// - The last row of each plane may stop after its samples (`len` ≥ (rows − 1) · stride + width).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Nv12 {
    pub size: PixelSize,
    pub y: Vec<u8>,
    pub y_stride: u32,
    pub uv: Vec<u8>,
    pub uv_stride: u32,
    pub colour: YuvColour,
}

impl Nv12 {
    /// Checks the dimensions, strides and plane lengths described on [`Nv12`].
    pub fn validate(&self) -> Result<(), CodecError> {
        let PixelSize { width, height, .. } = self.size;
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(CodecError::BadInput("NV12 size must be even and nonzero"));
        }
        if self.y_stride < width || self.uv_stride < width {
            return Err(CodecError::BadInput("NV12 stride shorter than a row"));
        }
        let fits = |plane: &[u8], rows: u32, stride: u32| {
            (u64::from(rows) - 1)
                .checked_mul(u64::from(stride))
                .and_then(|offset| offset.checked_add(u64::from(width)))
                .is_some_and(|needed| plane.len() as u64 >= needed)
        };
        if !fits(&self.y, height, self.y_stride) || !fits(&self.uv, height / 2, self.uv_stride) {
            return Err(CodecError::BadInput("NV12 plane shorter than its size"));
        }
        Ok(())
    }
}

/// A decoded picture that stays in native memory (an IOSurface on macOS), for the renderer to
/// import into the GPU without a copy (WP-2.24). Holding it keeps the decoder's buffer from being
/// reused; drop it once it's shown or superseded.
pub trait NativePicture: Send + Sync + fmt::Debug {
    /// The coded size (even dimensions, as for [`Nv12::size`]).
    fn size(&self) -> PixelSize;
    fn colour(&self) -> YuvColour;
    /// CPU fallback: copy the picture into `out` as NV12, reusing its allocations.
    fn to_nv12(&self, out: &mut Nv12) -> Result<(), CodecError>;
    /// The platform crate's concrete type, for its GPU importer.
    fn as_any(&self) -> &dyn Any;
}

/// What [`crate::codec::VideoDecoder::decode_native`] produced.
#[derive(Clone, Debug)]
pub enum Decoded {
    /// NV12 planes in CPU memory.
    Nv12(Arc<Nv12>),
    /// A picture in native memory.
    Native(Arc<dyn NativePicture>),
}

impl Decoded {
    /// The coded size.
    pub fn size(&self) -> PixelSize {
        match self {
            Decoded::Nv12(picture) => picture.size,
            Decoded::Native(picture) => picture.size(),
        }
    }
}

/// The reference conversion: the top-left `crop` of `picture` as BGRA8 rows of `crop.width * 4`
/// bytes (alpha 255) in `out`, which is replaced. Each pixel takes the chroma pair of its 2×2
/// block (no chroma filtering), is converted with `picture.colour.to_rgb()` in `f32`, then scaled
/// by 255, rounded to nearest and clamped. The renderer's shader matches this within ±1.
pub fn nv12_to_bgra(picture: &Nv12, crop: PixelSize, out: &mut Vec<u8>) -> Result<(), CodecError> {
    picture.validate()?;
    if crop.width > picture.size.width || crop.height > picture.size.height {
        return Err(CodecError::BadInput("crop larger than the picture"));
    }
    let YuvToRgb { offset, rows } = picture.colour.to_rgb();
    out.clear();
    out.reserve(crop.width as usize * crop.height as usize * 4);
    for y in 0..crop.height as usize {
        let luma = &picture.y[y * picture.y_stride as usize..][..crop.width as usize];
        let chroma = &picture.uv[(y / 2) * picture.uv_stride as usize..]
            [..crop.width.next_multiple_of(2) as usize];
        for (x, &sample) in luma.iter().enumerate() {
            let pair = &chroma[(x / 2) * 2..(x / 2) * 2 + 2];
            let yuv = [
                f32::from(sample) / 255.0 - offset[0],
                f32::from(pair[0]) / 255.0 - offset[1],
                f32::from(pair[1]) / 255.0 - offset[2],
            ];
            let [r, g, b] = rows.map(|row| {
                let value = row[0] * yuv[0] + row[1] * yuv[1] + row[2] * yuv[2];
                (value.clamp(0.0, 1.0) * 255.0).round() as u8
            });
            out.extend_from_slice(&[b, g, r, 255]);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(size: PixelSize, colour: YuvColour, sample: impl Fn(u32, u32) -> [u8; 3]) -> Nv12 {
        let mut nv12 = Nv12 {
            size,
            y: vec![0; (size.width * size.height) as usize],
            y_stride: size.width,
            uv: vec![0; (size.width * size.height / 2) as usize],
            uv_stride: size.width,
            colour,
        };
        for y in 0..size.height {
            for x in 0..size.width {
                let [luma, cb, cr] = sample(x, y);
                nv12.y[(y * size.width + x) as usize] = luma;
                let at = ((y / 2) * size.width + (x / 2) * 2) as usize;
                nv12.uv[at] = cb;
                nv12.uv[at + 1] = cr;
            }
        }
        nv12
    }

    fn convert(picture: &Nv12, crop: PixelSize) -> Vec<u8> {
        let mut out = Vec::new();
        nv12_to_bgra(picture, crop, &mut out).expect("convert");
        out
    }

    #[test]
    fn limited_range_black_and_white_hit_the_ends() {
        for matrix in [YuvMatrix::Bt601, YuvMatrix::Bt709] {
            let colour = YuvColour {
                matrix,
                full_range: false,
            };
            let size = PixelSize::new(2, 2);
            assert_eq!(
                convert(&picture(size, colour, |_, _| [16, 128, 128]), size)[..4],
                [0, 0, 0, 255]
            );
            assert_eq!(
                convert(&picture(size, colour, |_, _| [235, 128, 128]), size)[..4],
                [255, 255, 255, 255]
            );
            assert_eq!(
                convert(&picture(size, colour, |_, _| [5, 128, 128]), size)[..4],
                [0, 0, 0, 255]
            );
        }
    }

    #[test]
    fn bt709_limited_primaries() {
        // BT.709 limited-range codes for sRGB red, green and blue (H.273 forward transform).
        let colour = YuvColour::default();
        let size = PixelSize::new(2, 2);
        let red = convert(&picture(size, colour, |_, _| [63, 102, 240]), size);
        let green = convert(&picture(size, colour, |_, _| [173, 42, 26]), size);
        let blue = convert(&picture(size, colour, |_, _| [32, 240, 118]), size);
        let near =
            |got: &[u8], want: [u8; 3]| got[..3].iter().zip(want).all(|(&g, w)| g.abs_diff(w) <= 2);
        assert!(near(&red, [0, 0, 255]), "{red:?}");
        assert!(near(&green, [0, 255, 0]), "{green:?}");
        assert!(near(&blue, [255, 0, 0]), "{blue:?}");
    }

    #[test]
    fn full_range_grey_is_identity() {
        let colour = YuvColour {
            matrix: YuvMatrix::Bt709,
            full_range: true,
        };
        let size = PixelSize::new(256, 2);
        let out = convert(&picture(size, colour, |x, _| [x as u8, 128, 128]), size);
        for x in 0..256 {
            assert_eq!(out[x * 4..x * 4 + 4], [x as u8, x as u8, x as u8, 255]);
        }
    }

    #[test]
    fn chroma_is_shared_by_each_two_by_two_block_and_crop_skips_padding() {
        let colour = YuvColour {
            matrix: YuvMatrix::Bt709,
            full_range: true,
        };
        let size = PixelSize::new(4, 4);
        // Cb differs per block; the crop drops the padded last column and row.
        let nv12 = picture(size, colour, |x, y| {
            [128, if (x / 2 + y / 2) % 2 == 0 { 64 } else { 192 }, 128]
        });
        let out = convert(&nv12, PixelSize::new(3, 3));
        assert_eq!(out.len(), 3 * 3 * 4);
        let blue = |x: usize, y: usize| out[(y * 3 + x) * 4];
        assert_eq!(blue(0, 0), blue(1, 1));
        assert_ne!(blue(1, 0), blue(2, 0));
        assert_eq!(blue(2, 0), blue(2, 1));
        assert_ne!(blue(0, 1), blue(0, 2));
    }

    #[test]
    fn strided_planes_and_short_last_rows_are_accepted() {
        let colour = YuvColour::default();
        let size = PixelSize::new(2, 2);
        let nv12 = Nv12 {
            size,
            y: vec![16, 16, 0, 0, 235, 235],
            y_stride: 4,
            uv: vec![128, 128],
            uv_stride: 8,
            colour,
        };
        let out = convert(&nv12, size);
        assert_eq!(out[..4], [0, 0, 0, 255]);
        assert_eq!(out[8..12], [255, 255, 255, 255]);
    }

    #[test]
    fn invalid_pictures_are_refused() {
        let good = picture(PixelSize::new(4, 2), YuvColour::default(), |_, _| {
            [16, 128, 128]
        });
        assert!(good.validate().is_ok());
        let mut odd = good.clone();
        odd.size = PixelSize::new(3, 2);
        assert!(odd.validate().is_err());
        let mut empty = good.clone();
        empty.size = PixelSize::new(0, 0);
        assert!(empty.validate().is_err());
        let mut short = good.clone();
        short.uv.truncate(3);
        assert!(short.validate().is_err());
        let mut narrow = good.clone();
        narrow.y_stride = 2;
        assert!(narrow.validate().is_err());
        let mut out = Vec::new();
        assert!(nv12_to_bgra(&good, PixelSize::new(6, 2), &mut out).is_err());
    }
}
