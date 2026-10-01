//! CPU reference codec for tightly packed BGRA8 tiles.
//!
//! Changes are detected with xxh3-64 over concatenated pixel rows, excluding stride padding.
//! A 64-bit hash collision can suppress a change; this is accepted for E2 v0. Periodic key
//! frames repair it. Encoders belong to one projection; sequence/stale-frame policy is the
//! caller's responsibility.

use crosspane_types::geom::{PixelRect, PixelSize, euclid::point2};
use xxhash_rust::xxh3::xxh3_64;

use crate::wire::{
    Codec, FrameHeader, HEADER_BYTES, MAX_FRAME_BYTES, MediaError, RECORD_BYTES, Reader, TILE,
    parse_header, read_codec, tile_grid, write_header,
};

/// Maximum number of successful capture calls between key frames. Unchanged captures count.
const KEY_FRAME_INTERVAL: u32 = 300;

#[derive(Debug, Default)]
pub struct TileEncoder {
    size: Option<PixelSize>,
    hashes: Vec<u64>,
    frames_since_key: u32,
    key_requested: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeStats {
    pub key: bool,
    pub tiles: u32,
    pub bytes: usize,
}

impl TileEncoder {
    pub fn new() -> TileEncoder {
        Self::default()
    }

    /// Encode the next captured image. `out` is replaced, and is empty on `Ok(None)`.
    /// Only successful calls advance the hashes and periodic-key counter.
    pub fn encode(
        &mut self,
        mut header: FrameHeader,
        pixels: &[u8],
        stride: u32,
        force_key: bool,
        out: &mut Vec<u8>,
    ) -> Result<Option<EncodeStats>, MediaError> {
        let (tiles_x, tiles_y) = tile_grid(header.width, header.height)?;
        let row_bytes = header.width as usize * 4;
        let stride = stride as usize;
        if stride < row_bytes {
            return Err(MediaError::BadPayload);
        }
        // Padding after the last row is not read and need not be supplied.
        let required = (header.height as usize - 1)
            .checked_mul(stride)
            .and_then(|offset| offset.checked_add(row_bytes))
            .ok_or(MediaError::BadPayload)?;
        if pixels.len() < required {
            return Err(MediaError::BadPayload);
        }
        let size = PixelSize::new(header.width, header.height);
        let key = force_key
            || self.key_requested
            || self.size != Some(size)
            || self.frames_since_key >= KEY_FRAME_INTERVAL - 1;
        header.key = key;

        out.clear();
        write_header(header, 0, out);
        let mut hashes = Vec::with_capacity((tiles_x * tiles_y) as usize);
        let mut tile = Vec::with_capacity((TILE * TILE * 4) as usize);
        let mut count = 0_u32;
        for ty in 0..tiles_y {
            for tx in 0..tiles_x {
                let geometry = TileGeometry::new(header, tx, ty);
                tile.clear();
                for y in geometry.y..geometry.y + geometry.height {
                    // The checked required length above bounds all row offsets, even on 32-bit.
                    let start = y as usize * stride + geometry.x as usize * 4;
                    let row = pixels
                        .get(start..start + geometry.row_bytes())
                        .ok_or(MediaError::BadPayload)?;
                    tile.extend_from_slice(row);
                }
                let hash = xxh3_64(&tile);
                let changed = key || self.hashes.get(hashes.len()) != Some(&hash);
                hashes.push(hash);
                if !changed {
                    continue;
                }

                let first_pixel = tile.get(..4).ok_or(MediaError::BadPayload)?;
                let solid = tile
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .all(|pixel| pixel == first_pixel);
                let compressed;
                let (encoding, payload) = if solid {
                    (2, first_pixel)
                } else {
                    compressed = lz4_flex::compress_prepend_size(&tile);
                    if compressed.len() > tile.len() {
                        (0, tile.as_slice())
                    } else {
                        (1, compressed.as_slice())
                    }
                };
                if out.len() + RECORD_BYTES + payload.len() > MAX_FRAME_BYTES {
                    out.clear();
                    return Err(MediaError::TooLarge);
                }
                out.extend_from_slice(&(tx as u16).to_le_bytes());
                out.extend_from_slice(&(ty as u16).to_le_bytes());
                out.extend_from_slice(&[encoding, 0, 0, 0]);
                out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                out.extend_from_slice(payload);
                count += 1;
            }
        }
        out.get_mut(44..HEADER_BYTES)
            .ok_or(MediaError::Truncated)?
            .copy_from_slice(&count.to_le_bytes());
        self.size = Some(size);
        self.hashes = hashes;
        self.key_requested = false;
        self.frames_since_key = if key { 0 } else { self.frames_since_key + 1 };
        if !key && count == 0 {
            out.clear();
            return Ok(None);
        }
        Ok(Some(EncodeStats {
            key,
            tiles: count,
            bytes: out.len(),
        }))
    }

    /// Force the next successfully encoded frame to contain every tile.
    pub fn request_key(&mut self) {
        self.key_requested = true;
    }
}

#[derive(Debug, Default)]
pub struct TileDecoder {
    pixels: Vec<u8>,
    size: PixelSize,
}

impl TileDecoder {
    pub fn new() -> TileDecoder {
        Self::default()
    }

    /// Validate the whole payload, then apply it. An error leaves the canvas unchanged.
    pub fn apply(&mut self, data: &[u8]) -> Result<(FrameHeader, Vec<PixelRect>), MediaError> {
        if read_codec(data)? != Codec::Tiles {
            return Err(MediaError::BadCodec);
        }
        let (header, count) = parse_header(data)?;
        let size = PixelSize::new(header.width, header.height);
        if !header.key {
            if self.pixels.is_empty() {
                return Err(MediaError::NoCanvas);
            }
            if self.size != size {
                return Err(MediaError::SizeMismatch);
            }
        }
        let (tiles_x, tiles_y) = tile_grid(header.width, header.height)?;
        let mut input = Reader::new(data.get(HEADER_BYTES..).ok_or(MediaError::Truncated)?);
        // Reject impossible counts before allocating any tile storage.
        if count as usize > input.remaining().len() / RECORD_BYTES {
            return Err(MediaError::Truncated);
        }
        let mut seen = vec![false; (tiles_x * tiles_y) as usize];
        let mut tiles = Vec::with_capacity(count as usize);
        let mut rects = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let tx = u32::from(input.u16()?);
            let ty = u32::from(input.u16()?);
            if tx >= tiles_x || ty >= tiles_y {
                return Err(MediaError::BadTile);
            }
            let visited = seen
                .get_mut((ty * tiles_x + tx) as usize)
                .ok_or(MediaError::BadTile)?;
            if *visited {
                return Err(MediaError::Duplicate);
            }
            *visited = true;
            let encoding = input.u8()?;
            if input.take(3)? != [0, 0, 0] {
                return Err(MediaError::BadReserved);
            }
            let len = input.u32()? as usize;
            let geometry = TileGeometry::new(header, tx, ty);
            let expected = geometry.byte_len();
            if (encoding == 0 && len != expected) || (encoding == 2 && len != 4) {
                return Err(MediaError::BadPayload);
            }
            let payload = input.take(len)?;
            let pixels = match encoding {
                0 => TilePixels::Packed(payload),
                1 => {
                    let mut block = Reader::new(payload);
                    // Never let an attacker-controlled LZ4 size determine the allocation.
                    if block.u32().map_err(|_| MediaError::BadPayload)? as usize != expected {
                        return Err(MediaError::BadPayload);
                    }
                    let mut decoded = vec![0; expected];
                    let written = lz4_flex::block::decompress_into(block.remaining(), &mut decoded)
                        .map_err(|_| MediaError::BadPayload)?;
                    if written != expected {
                        return Err(MediaError::BadPayload);
                    }
                    TilePixels::Decoded(decoded)
                }
                2 => TilePixels::Solid(payload.try_into().map_err(|_| MediaError::BadPayload)?),
                _ => return Err(MediaError::BadPayload),
            };
            rects.push(geometry.rect());
            tiles.push(ValidatedTile { geometry, pixels });
        }
        if !input.remaining().is_empty() {
            return Err(MediaError::Trailing);
        }
        // Unique in-range indices plus an exact key count prove that every tile is present.
        // All potentially failing operations finish before touching the existing canvas.
        if header.key {
            let len = header.width as usize * header.height as usize * 4;
            let mut canvas = Vec::new();
            canvas
                .try_reserve_exact(len)
                .map_err(|_| MediaError::TooLarge)?;
            canvas.resize(len, 0);
            for tile in tiles {
                tile.apply(&mut canvas, header.width);
            }
            self.pixels = canvas;
            self.size = size;
        } else {
            for tile in tiles {
                tile.apply(&mut self.pixels, header.width);
            }
        }
        Ok((header, rects))
    }

    /// BGRA8 canvas, tightly packed; empty (with size 0×0) before the first key frame.
    pub fn canvas(&self) -> (&[u8], PixelSize) {
        (&self.pixels, self.size)
    }
}

struct TileGeometry {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl TileGeometry {
    // Callers have validated dimensions and indices before constructing geometry.
    fn new(header: FrameHeader, tx: u32, ty: u32) -> Self {
        let x = tx * TILE;
        let y = ty * TILE;
        Self {
            x,
            y,
            width: TILE.min(header.width - x),
            height: TILE.min(header.height - y),
        }
    }

    fn row_bytes(&self) -> usize {
        self.width as usize * 4
    }

    fn byte_len(&self) -> usize {
        self.row_bytes() * self.height as usize
    }

    fn rect(&self) -> PixelRect {
        PixelRect::new(
            point2(self.x as i32, self.y as i32),
            point2((self.x + self.width) as i32, (self.y + self.height) as i32),
        )
    }
}

enum TilePixels<'a> {
    Packed(&'a [u8]),
    Decoded(Vec<u8>),
    Solid([u8; 4]),
}

struct ValidatedTile<'a> {
    geometry: TileGeometry,
    pixels: TilePixels<'a>,
}

impl ValidatedTile<'_> {
    fn apply(self, canvas: &mut [u8], width: u32) {
        let geometry = self.geometry;
        let x = geometry.x as usize * 4;
        let rows = canvas
            .chunks_exact_mut(width as usize * 4)
            .skip(geometry.y as usize)
            .take(geometry.height as usize);
        match self.pixels {
            TilePixels::Solid(pixel) => {
                for row in rows {
                    // Validated geometry and canvas dimensions guarantee this range exists.
                    if let Some(destination) = row.get_mut(x..x + geometry.row_bytes()) {
                        for output_pixel in destination.as_chunks_mut::<4>().0 {
                            output_pixel.copy_from_slice(&pixel);
                        }
                    }
                }
            }
            packed => {
                let bytes = match &packed {
                    TilePixels::Packed(bytes) => *bytes,
                    TilePixels::Decoded(bytes) => bytes.as_slice(),
                    TilePixels::Solid(_) => return,
                };
                for (row, source) in rows.zip(bytes.chunks_exact(geometry.row_bytes())) {
                    if let Some(destination) = row.get_mut(x..x + geometry.row_bytes()) {
                        destination.copy_from_slice(source);
                    }
                }
            }
        }
    }
}
