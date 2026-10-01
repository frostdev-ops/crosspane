// Frozen pre-WP encoder, for byte-for-byte output regression checks.
use crosspane_media::{
    tiles::EncodeStats,
    wire::{FrameHeader, MAGIC, MAX_FRAME_BYTES, MediaError, TILE},
};
use crosspane_types::geom::PixelSize;
use xxhash_rust::xxh3::xxh3_64;
const HEADER_BYTES: usize = 48;
const RECORD_BYTES: usize = 12;
fn tile_grid(width: u32, height: u32) -> Result<(u32, u32), MediaError> {
    if !(1..=16384).contains(&width) || !(1..=16384).contains(&height) {
        return Err(MediaError::BadSize);
    }
    Ok((width.div_ceil(TILE), height.div_ceil(TILE)))
}
fn write_header(h: FrameHeader, count: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&[1, u8::from(h.key), 0, 0]);
    out.extend_from_slice(&h.projection.to_le_bytes());
    out.extend_from_slice(&h.seq.to_le_bytes());
    out.extend_from_slice(&h.captured_ns.to_le_bytes());
    out.extend_from_slice(&h.width.to_le_bytes());
    out.extend_from_slice(&h.height.to_le_bytes());
    out.extend_from_slice(&(TILE as u16).to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
}
/// Maximum number of successful capture calls between key frames. Unchanged captures count.
const KEY_FRAME_INTERVAL: u32 = 300;

#[derive(Debug, Default)]
pub struct LegacyEncoder {
    size: Option<PixelSize>,
    hashes: Vec<u64>,
    frames_since_key: u32,
    key_requested: bool,
}

impl LegacyEncoder {
    pub fn new() -> LegacyEncoder {
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
}
