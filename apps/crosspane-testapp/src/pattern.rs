//! Integer pixel pattern and binary PPM reference writer.

use std::{io::Write, num::NonZeroU32, str::FromStr};

use anyhow::{Context, Result};

/// A nonempty image size, also used for the CLI's logical window size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageSize {
    width: NonZeroU32,
    height: NonZeroU32,
}

impl ImageSize {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        Ok(Self {
            width: NonZeroU32::new(width).context("width must be positive")?,
            height: NonZeroU32::new(height).context("height must be positive")?,
        })
    }

    pub fn width(self) -> u32 {
        self.width.get()
    }

    pub fn height(self) -> u32 {
        self.height.get()
    }

    pub fn byte_len(self, channels: u32) -> Result<usize> {
        let len = u64::from(self.width())
            .checked_mul(u64::from(self.height()))
            .and_then(|pixels| pixels.checked_mul(u64::from(channels)))
            .context("image byte count overflows")?;
        usize::try_from(len).context("image does not fit in memory")
    }
}

impl FromStr for ImageSize {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let (width, height) = value.split_once('x').context("size must be WxH")?;
        Self::new(
            width.parse().context("invalid width")?,
            height.parse().context("invalid height")?,
        )
    }
}

const BARS: [[u8; 3]; 8] = [
    [255, 255, 255],
    [255, 255, 0],
    [0, 255, 255],
    [0, 255, 0],
    [255, 0, 255],
    [255, 0, 0],
    [0, 0, 255],
    [0, 0, 0],
];

fn ramp_value(x: u32, width: u32) -> u8 {
    if width == 1 {
        0
    } else {
        (u64::from(x) * 255 / u64::from(width - 1)) as u8
    }
}

fn bar_color(x: u32, width: u32) -> [u8; 3] {
    BARS[(u64::from(x) * 8 / u64::from(width)) as usize]
}

fn pixel(size: ImageSize, x: u32, y: u32) -> [u8; 3] {
    if x == 0 || y == 0 || x == size.width() - 1 || y == size.height() - 1 {
        [255, 0, 0]
    } else if x < 256 && y < 256 {
        if (x + y).is_multiple_of(2) {
            [255, 255, 255]
        } else {
            [0, 0, 0]
        }
    } else if y >= size.height().saturating_sub(32) {
        [ramp_value(x, size.width()); 3]
    } else {
        bar_color(x, size.width())
    }
}

/// Produce tightly packed RGB bytes in top-to-bottom, left-to-right order.
pub fn rgb_pattern(size: ImageSize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size.byte_len(3)?)
        .context("allocating CPU reference image")?;
    for y in 0..size.height() {
        for x in 0..size.width() {
            bytes.extend_from_slice(&pixel(size, x, y));
        }
    }
    Ok(bytes)
}

/// Write the exact P6 header and RGB payload, without trailing bytes.
pub fn write_ppm(mut writer: impl Write, size: ImageSize) -> Result<()> {
    let bytes = rgb_pattern(size)?;
    write!(writer, "P6\n{} {}\n255\n", size.width(), size.height())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn check_corners(size: ImageSize) {
        for (x, y) in [
            (0, 0),
            (size.width() - 1, 0),
            (0, size.height() - 1),
            (size.width() - 1, size.height() - 1),
        ] {
            assert_eq!(pixel(size, x, y), [255, 0, 0]);
        }
    }

    #[test]
    fn one_by_one() {
        let size = ImageSize::new(1, 1).unwrap();
        check_corners(size);
        assert_eq!(rgb_pattern(size).unwrap(), [255, 0, 0]);
        assert_eq!(ramp_value(0, 1), 0);
    }

    #[test]
    fn two_by_two() {
        let size = ImageSize::new(2, 2).unwrap();
        check_corners(size);
        assert_eq!(rgb_pattern(size).unwrap(), [255, 0, 0].repeat(4));
    }

    #[test]
    fn nine_by_forty() {
        let size = ImageSize::new(9, 40).unwrap();
        check_corners(size);
        assert_eq!(pixel(size, 1, 1), [255, 255, 255]);
        assert_eq!(pixel(size, 2, 1), [0, 0, 0]);
        assert_eq!(pixel(size, 1, 2), [0, 0, 0]);
        // The checkerboard takes precedence even within the final 32 rows.
        assert_eq!(pixel(size, 4, 38), [255, 255, 255]);
        assert_eq!(pixel(size, 3, 38), [0, 0, 0]);
        // The border masks the ramp endpoints in the composed pattern.
        assert_eq!(ramp_value(0, 9), 0);
        assert_eq!(ramp_value(8, 9), 255);
    }

    #[test]
    fn three_hundred_by_two_hundred() {
        let size = ImageSize::new(300, 200).unwrap();
        check_corners(size);
        assert_eq!(pixel(size, 254, 1), [0, 0, 0]);
        assert_eq!(pixel(size, 255, 1), [255, 255, 255]);
        assert_eq!(pixel(size, 256, 167), [0, 0, 255]);
        assert_eq!(pixel(size, 262, 167), [0, 0, 255]);
        assert_eq!(pixel(size, 263, 167), [0, 0, 0]);
        assert_eq!(pixel(size, 255, 168), [0, 0, 0]);
        assert_eq!(pixel(size, 256, 168), [218, 218, 218]);
        assert_eq!(pixel(size, 298, 198), [254, 254, 254]);
        assert_eq!(pixel(size, 299, 198), [255, 0, 0]);
        assert_eq!(pixel(size, 256, 199), [255, 0, 0]);
        assert_eq!(ramp_value(0, 300), 0);
        assert_eq!(ramp_value(299, 300), 255);

        // Check all bar colours independently of the checkerboard masking bars 0..5.
        for (x, expected) in [
            (1, [255, 255, 255]),
            (38, [255, 255, 0]),
            (75, [0, 255, 255]),
            (113, [0, 255, 0]),
            (150, [255, 0, 255]),
            (188, [255, 0, 0]),
            (225, [0, 0, 255]),
            (263, [0, 0, 0]),
        ] {
            assert_eq!(bar_color(x, 300), expected);
        }
    }

    #[test]
    fn ppm_header_and_payload() {
        for size in [(1, 1), (2, 2), (9, 40), (300, 200)] {
            let size = ImageSize::new(size.0, size.1).unwrap();
            let mut ppm = Vec::new();
            write_ppm(&mut ppm, size).unwrap();
            let header = format!("P6\n{} {}\n255\n", size.width(), size.height());
            assert_eq!(&ppm[..header.len()], header.as_bytes());
            assert_eq!(ppm.len(), header.len() + size.byte_len(3).unwrap());
            assert_eq!(&ppm[header.len()..], rgb_pattern(size).unwrap());
        }
        let mut ppm = Vec::new();
        write_ppm(&mut ppm, ImageSize::new(9, 40).unwrap()).unwrap();
        assert_eq!(&ppm[..12], b"P6\n9 40\n255\n");
        assert_eq!(ppm.len(), 1092);
    }
}
