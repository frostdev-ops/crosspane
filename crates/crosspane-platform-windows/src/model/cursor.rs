//! Bounded cursor conversion and per-stream change detection. No native handles or calls.
use crosspane_platform::CursorImage;
use crosspane_types::geom::{PixelRect, PixelSize};

#[derive(Debug)]
pub struct Rows<'a> {
    pub stride: usize,
    pub pixels: &'a [u8],
}

/// One-bit, most-significant-bit first, top-row-first AND/XOR data.
#[derive(Debug)]
pub struct Mask<'a> {
    pub size: PixelSize,
    pub rows: Rows<'a>,
}

impl Rows<'_> {
    fn valid(&self, row: usize, height: u32) -> bool {
        height > 0
            && self.stride >= row
            && (height as usize - 1)
                .checked_mul(self.stride)
                .and_then(|n| n.checked_add(row))
                .is_some_and(|n| n <= self.pixels.len())
    }
}
impl Mask<'_> {
    fn bit(&self, x: u32, y: u32) -> bool {
        self.rows.pixels[y as usize * self.rows.stride + x as usize / 8] & (0x80 >> (x % 8)) != 0
    }
}

/// Colour bytes are Win32 premultiplied BGRA. All-zero alpha selects the legacy AND mask.
/// Monochrome AND=1/XOR=1 cannot be encoded without the destination background; use opaque
/// neutral gray for that inversion case. Source rows and allocation are bounded before access.
pub fn convert(
    size: PixelSize,
    hotspot: (u32, u32),
    colour: Option<Rows<'_>>,
    mask: Mask<'_>,
    density: (u32, u32),
) -> Option<CursorImage> {
    if size.width == 0
        || size.height == 0
        || size.width.max(size.height) > 4096
        || density.0 == 0
        || density.1 == 0
        || mask.size.width != size.width
        || mask.size.height != size.height * if colour.is_some() { 1 } else { 2 }
        || !mask
            .rows
            .valid(size.width.div_ceil(8) as usize, mask.size.height)
    {
        return None;
    }
    if colour
        .as_ref()
        .is_some_and(|r| !r.valid(size.width as usize * 4, size.height))
    {
        return None;
    }
    let alpha = colour.as_ref().is_some_and(|rows| {
        (0..size.height).any(|y| {
            rows.pixels
                [y as usize * rows.stride..y as usize * rows.stride + size.width as usize * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[3] != 0)
        })
    });
    let width = (u64::from(size.width) * u64::from(density.0) / u64::from(density.1)).max(1);
    let height = (u64::from(size.height) * u64::from(density.0) / u64::from(density.1)).max(1);
    let longest = width.max(height);
    let fit = |n: u64| {
        if longest > 256 {
            (n * 256 / longest).max(1) as u32
        } else {
            n as u32
        }
    };
    let output = PixelSize::new(fit(width), fit(height));
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(output.width as usize * output.height as usize * 4)
        .ok()?;
    for y in 0..output.height {
        let sy = (u64::from(y) * u64::from(size.height) / u64::from(output.height)) as u32;
        for x in 0..output.width {
            let sx = (u64::from(x) * u64::from(size.width) / u64::from(output.width)) as u32;
            let pixel = if let Some(rows) = &colour {
                let at = sy as usize * rows.stride + sx as usize * 4;
                let p = &rows.pixels[at..at + 4];
                let a = if alpha {
                    p[3]
                } else if mask.bit(sx, sy) {
                    0
                } else {
                    255
                };
                if a == 0 {
                    [0; 4]
                } else {
                    let channel = |c: u8| {
                        ((u32::from(c) * 255 + u32::from(a) / 2) / u32::from(a)).min(255) as u8
                    };
                    [channel(p[0]), channel(p[1]), channel(p[2]), a]
                }
            } else {
                match (mask.bit(sx, sy), mask.bit(sx, sy + size.height)) {
                    (false, false) => [0, 0, 0, 255],
                    (false, true) => [255; 4],
                    (true, false) => [0; 4],
                    (true, true) => [128, 128, 128, 255],
                }
            };
            pixels.extend_from_slice(&pixel);
        }
    }
    let spot = |coordinate: u32, source: u32, dest: u32| {
        (u64::from(coordinate.min(source - 1)) * u64::from(dest) / u64::from(source)) as u32
    };
    Some(CursorImage {
        size: output,
        hotspot: (
            spot(hotspot.0, size.width, output.width),
            spot(hotspot.1, size.height, output.height),
        ),
        pixels: pixels.into(),
    })
}

/// DWM extended bounds and WGC are physical pixels. A pending resize is not a scale factor:
/// wait for matching extents instead of attributing an old crop to a new screen rectangle.
pub fn over_content(
    point: (i32, i32),
    bounds: [i32; 4],
    captured: PixelSize,
    crop: Option<PixelRect>,
    same_root: bool,
) -> bool {
    let width = i64::from(bounds[2]) - i64::from(bounds[0]);
    let height = i64::from(bounds[3]) - i64::from(bounds[1]);
    let x = i64::from(point.0) - i64::from(bounds[0]);
    let y = i64::from(point.1) - i64::from(bounds[1]);
    same_root
        && width > 0
        && height > 0
        && width == i64::from(captured.width)
        && height == i64::from(captured.height)
        && x >= 0
        && y >= 0
        && x < width
        && y < height
        && crop.is_none_or(|r| {
            !r.is_empty()
                && x >= i64::from(r.min.x)
                && y >= i64::from(r.min.y)
                && x < i64::from(r.max.x)
                && y < i64::from(r.max.y)
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shape {
    Image(CursorImage),
    Hidden,
    Default,
}

#[derive(Debug, Default)]
pub struct History {
    last: Option<Shape>,
}
impl History {
    pub fn observe(&mut self, over: bool, shape: Shape) -> Option<Shape> {
        if !over {
            self.last = None;
            return None;
        }
        if self.last.as_ref() == Some(&shape) {
            return None;
        }
        self.last = Some(shape.clone());
        Some(shape)
    }
}

#[derive(Debug, Default)]
pub struct Cache {
    last: Option<(usize, u64, Option<CursorImage>)>,
}
impl Cache {
    pub fn read(
        &mut self,
        handle: usize,
        content: u64,
        render: impl FnOnce() -> Option<CursorImage>,
    ) -> Option<CursorImage> {
        if let Some((old_handle, old_content, image)) = &self.last
            && *old_handle == handle
            && *old_content == content
        {
            return image.clone();
        }
        let image = render();
        self.last = Some((handle, content, image.clone()));
        image
    }
}
