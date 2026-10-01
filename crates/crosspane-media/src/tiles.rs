//! CPU reference codec for tightly packed BGRA8 tiles.
//!
//! Changes are detected with xxh3-64 over each tile's concatenated pixel rows, excluding stride
//! padding. [`TileEncoder::scan`] only hashes; LZ4 runs in [`TileEncoder::emit`] for the tiles it
//! sends, so a capture that goes out as video costs a scan and a [`TileEncoder::commit`].
//! A 64-bit hash collision can suppress a change; this is accepted for E2 v0. Periodic key
//! frames repair it. Encoders belong to one projection; sequence/stale-frame policy is the
//! caller's responsibility.

use std::sync::Arc;

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
    // Replaced on every successful emit/commit; outstanding scans keep their token alive.
    generation: Arc<()>,
}

/// The tile changes of one capture, from [`TileEncoder::scan`] or
/// [`TileEncoder::scan_external`]. Hand it to [`TileEncoder::emit`]
/// or [`TileEncoder::commit`] before the next scan.
#[derive(Debug)]
pub struct TileScan {
    size: PixelSize,
    kind: ScanKind,
    total: u32,
    changed: u32,
    generation: Arc<()>,
}

#[derive(Debug)]
enum ScanKind {
    Cpu(Vec<u64>),
    External(Vec<u32>),
}

impl ScanKind {
    fn into_hashes(self) -> Vec<u64> {
        match self {
            Self::Cpu(hashes) => hashes,
            Self::External(_) => Vec::new(),
        }
    }
}

impl TileScan {
    /// Tiles that differ from the last committed capture: all of them after a size change or
    /// before the first commit.
    pub fn changed(&self) -> u32 {
        self.changed
    }

    /// Tiles in the capture.
    pub fn total(&self) -> u32 {
        self.total
    }
}

/// Packed tiles of one capture, for encoding from a GPU readback that holds only some tiles
/// (WP-2.24). Tile `(tx, ty)` is its pixels as BGRA8 rows of `w * 4` bytes, `h` rows, no padding,
/// where `w` × `h` is the tile's size inside the image (edge tiles are smaller). `None` when the
/// source doesn't hold that tile.
pub trait TileSource {
    fn tile(&self, tx: u32, ty: u32) -> Option<&[u8]>;
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
        header: FrameHeader,
        pixels: &[u8],
        stride: u32,
        force_key: bool,
        out: &mut Vec<u8>,
    ) -> Result<Option<EncodeStats>, MediaError> {
        let scan = self.scan(PixelSize::new(header.width, header.height), pixels, stride)?;
        self.emit(scan, header, pixels, stride, force_key, out)
    }

    /// Hash the tiles of a capture (`size` pixels, BGRA8 rows of `stride` bytes; padding after the
    /// last row need not be supplied) and compare them with the last committed capture. Doesn't
    /// change the encoder.
    pub fn scan(
        &self,
        size: PixelSize,
        pixels: &[u8],
        stride: u32,
    ) -> Result<TileScan, MediaError> {
        let (tiles_x, tiles_y) = tile_grid(size.width, size.height)?;
        validate_pixels(size, pixels, stride)?;
        let mut hashes = Vec::with_capacity((tiles_x * tiles_y) as usize);
        let mut changed = 0;
        // Gathering a tile into this cache-resident buffer and hashing it in one shot measured
        // faster than streaming the rows through `Xxh3` (0.87 vs 1.13 ms at 3440×1440).
        let mut tile = Vec::with_capacity((TILE * TILE * 4) as usize);
        for ty in 0..tiles_y {
            for tx in 0..tiles_x {
                let geometry = TileGeometry::for_size(size, tx, ty);
                tile.clear();
                for y in geometry.y..geometry.y + geometry.height {
                    let start = y as usize * stride as usize + geometry.x as usize * 4;
                    tile.extend_from_slice(&pixels[start..start + geometry.row_bytes()]);
                }
                let hash = xxh3_64(&tile);
                if self.size != Some(size) || self.hashes.get(hashes.len()) != Some(&hash) {
                    changed += 1;
                }
                hashes.push(hash);
            }
        }
        Ok(TileScan {
            size,
            kind: ScanKind::Cpu(hashes),
            total: tiles_x * tiles_y,
            changed,
            generation: Arc::clone(&self.generation),
        })
    }

    /// A scan whose change detection ran elsewhere (the source GPU, WP-2.25). `changed_bits` is a
    /// row-major tile bitmap for `size`'s 64×64 grid: tile `i = ty * tiles_x + tx` is bit
    /// `i % 32` of word `i / 32`; exactly `ceil(tiles / 32)` words, and bits past the last tile
    /// must be zero (`Err(MediaError::BadPayload)` otherwise). The bits must be relative to the
    /// capture of the last `emit`/`emit_from`/`commit`; the caller keeps its detector in step.
    /// After a size change or before the first commit every tile counts as changed, whatever the
    /// bits say. Doesn't change the encoder.
    pub fn scan_external(
        &self,
        size: PixelSize,
        changed_bits: &[u32],
    ) -> Result<TileScan, MediaError> {
        let (tiles_x, tiles_y) = tile_grid(size.width, size.height)?;
        let total = tiles_x * tiles_y;
        if changed_bits.len() != total.div_ceil(32) as usize
            || (total % 32 != 0
                && changed_bits
                    .last()
                    .is_some_and(|word| word >> (total % 32) != 0))
        {
            return Err(MediaError::BadPayload);
        }
        let mut bits = changed_bits.to_vec();
        if self.size != Some(size) {
            bits.fill(u32::MAX);
            if total % 32 != 0
                && let Some(last) = bits.last_mut()
            {
                *last = (1 << (total % 32)) - 1;
            }
        }
        let changed = bits.iter().map(|word| word.count_ones()).sum();
        Ok(TileScan {
            size,
            kind: ScanKind::External(bits),
            total,
            changed,
            generation: Arc::clone(&self.generation),
        })
    }

    /// Encode a scanned capture into `out` (replaced) and commit its hashes. `pixels` and `stride`
    /// must be the scanned image, and `header.width`/`height` its size. It's a key frame (all
    /// tiles) when `force_key`, a requested key, a size change or the periodic interval calls for
    /// one. Otherwise it holds only the changed tiles, and is `Ok(None)` with `out` empty when none
    /// changed. A scan taken before the last `emit`/`commit`, or of another size than `header`, is
    /// `Err(MediaError::BadPayload)` and changes nothing.
    pub fn emit(
        &mut self,
        scan: TileScan,
        header: FrameHeader,
        pixels: &[u8],
        stride: u32,
        force_key: bool,
        out: &mut Vec<u8>,
    ) -> Result<Option<EncodeStats>, MediaError> {
        self.validate_scan(&scan, header)?;
        validate_pixels(scan.size, pixels, stride)?;
        self.emit_tiles(
            scan,
            header,
            TileInput::Strided { pixels, stride },
            force_key,
            out,
        )
    }

    /// `emit`, reading tiles from `tiles` instead of a strided image. A key frame reads every
    /// tile; otherwise only the changed ones. A tile the source doesn't have, or whose length
    /// isn't its packed size, is `Err(MediaError::BadPayload)`, leaves `out` empty and changes
    /// nothing. Same output bytes as `emit` for the same pixels and scan. Validation errors
    /// before writing (a stale scan or header size mismatch) preserve `out`.
    pub fn emit_from(
        &mut self,
        scan: TileScan,
        header: FrameHeader,
        tiles: &dyn TileSource,
        force_key: bool,
        out: &mut Vec<u8>,
    ) -> Result<Option<EncodeStats>, MediaError> {
        self.validate_scan(&scan, header)?;
        self.emit_tiles(scan, header, TileInput::Packed(tiles), force_key, out)
    }

    /// Whether the next `emit`/`emit_from` of a capture of `size` will be a key frame even
    /// without `force_key` (requested, periodic, size change, or nothing committed yet).
    pub fn key_pending(&self, size: PixelSize) -> bool {
        self.key_requested
            || self.size != Some(size)
            || self.frames_since_key >= KEY_FRAME_INTERVAL - 1
    }

    fn validate_scan(&self, scan: &TileScan, header: FrameHeader) -> Result<(), MediaError> {
        if !Arc::ptr_eq(&scan.generation, &self.generation)
            || scan.size != PixelSize::new(header.width, header.height)
        {
            return Err(MediaError::BadPayload);
        }
        tile_grid(header.width, header.height)?;
        Ok(())
    }

    fn emit_tiles(
        &mut self,
        scan: TileScan,
        mut header: FrameHeader,
        input: TileInput<'_>,
        force_key: bool,
        out: &mut Vec<u8>,
    ) -> Result<Option<EncodeStats>, MediaError> {
        let size = scan.size;
        let (tiles_x, tiles_y) = tile_grid(size.width, size.height)?;
        let key = force_key || self.key_pending(size);
        header.key = key;
        out.clear();
        write_header(header, 0, out);
        let result = (|| {
            let mut scratch = Vec::with_capacity((TILE * TILE * 4) as usize);
            let mut count = 0_u32;
            for ty in 0..tiles_y {
                for tx in 0..tiles_x {
                    let index = (ty * tiles_x + tx) as usize;
                    let changed = match &scan.kind {
                        ScanKind::Cpu(hashes) => self.hashes.get(index) != hashes.get(index),
                        ScanKind::External(bits) => bits[index / 32] & (1 << (index % 32)) != 0,
                    };
                    if !key && !changed {
                        continue;
                    }
                    let geometry = TileGeometry::for_size(size, tx, ty);
                    let tile = input.tile(tx, ty, &geometry, &mut scratch)?;
                    write_tile_record(tx, ty, tile, out)?;
                    count += 1;
                }
            }
            out.get_mut(44..HEADER_BYTES)
                .ok_or(MediaError::Truncated)?
                .copy_from_slice(&count.to_le_bytes());
            Ok(count)
        })();
        let count = match result {
            Ok(count) => count,
            Err(error) => {
                out.clear();
                return Err(error);
            }
        };
        self.size = Some(size);
        self.hashes = scan.kind.into_hashes();
        self.generation = Arc::new(());
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

    /// Commit a scanned capture's hashes without encoding it (it went out as video). It counts
    /// toward the periodic key frame, and requests a key frame for the next `emit`, because the
    /// destination's canvas no longer matches. A stale scan (as for `emit`) is
    /// `Err(MediaError::BadPayload)` and changes nothing.
    pub fn commit(&mut self, scan: TileScan) -> Result<(), MediaError> {
        if !Arc::ptr_eq(&scan.generation, &self.generation) {
            return Err(MediaError::BadPayload);
        }
        self.size = Some(scan.size);
        self.hashes = scan.kind.into_hashes();
        self.frames_since_key = self.frames_since_key.saturating_add(1);
        self.key_requested = true;
        self.generation = Arc::new(());
        Ok(())
    }

    /// Force the next successfully encoded frame to contain every tile.
    pub fn request_key(&mut self) {
        self.key_requested = true;
    }
}

enum TileInput<'a> {
    Strided { pixels: &'a [u8], stride: u32 },
    Packed(&'a dyn TileSource),
}

impl TileInput<'_> {
    fn tile<'a>(
        &'a self,
        tx: u32,
        ty: u32,
        geometry: &TileGeometry,
        scratch: &'a mut Vec<u8>,
    ) -> Result<&'a [u8], MediaError> {
        match self {
            Self::Packed(source) => {
                let tile = source.tile(tx, ty).ok_or(MediaError::BadPayload)?;
                if tile.len() != geometry.byte_len() {
                    return Err(MediaError::BadPayload);
                }
                Ok(tile)
            }
            Self::Strided { pixels, stride } => {
                scratch.clear();
                for y in geometry.y..geometry.y + geometry.height {
                    // Validated pixel geometry bounds all row offsets, even on 32-bit.
                    let start = y as usize * *stride as usize + geometry.x as usize * 4;
                    let row = pixels
                        .get(start..start + geometry.row_bytes())
                        .ok_or(MediaError::BadPayload)?;
                    scratch.extend_from_slice(row);
                }
                Ok(scratch)
            }
        }
    }
}

fn write_tile_record(tx: u32, ty: u32, tile: &[u8], out: &mut Vec<u8>) -> Result<(), MediaError> {
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
        compressed = lz4_flex::compress_prepend_size(tile);
        if compressed.len() > tile.len() {
            (0, tile)
        } else {
            (1, compressed.as_slice())
        }
    };
    if out.len() + RECORD_BYTES + payload.len() > MAX_FRAME_BYTES {
        return Err(MediaError::TooLarge);
    }
    out.extend_from_slice(&(tx as u16).to_le_bytes());
    out.extend_from_slice(&(ty as u16).to_le_bytes());
    out.extend_from_slice(&[encoding, 0, 0, 0]);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

#[derive(Debug, Default)]
pub struct TileDecoder {
    pixels: Arc<[u8]>,
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
        if self.size != size {
            let len = header.width as usize * header.height as usize * 4;
            let mut canvas = Vec::new();
            canvas
                .try_reserve_exact(len)
                .map_err(|_| MediaError::TooLarge)?;
            canvas.resize(len, 0);
            self.pixels = Arc::from(canvas);
        } else if Arc::get_mut(&mut self.pixels).is_none() {
            self.pixels = Arc::from(self.pixels.as_ref());
        }
        let canvas = Arc::get_mut(&mut self.pixels).ok_or(MediaError::BadPayload)?;
        for tile in tiles {
            tile.apply(canvas, header.width);
        }
        self.size = size;
        Ok((header, rects))
    }

    /// BGRA8 canvas, tightly packed; empty (with size 0×0) before the first key frame.
    pub fn canvas(&self) -> (&[u8], PixelSize) {
        (&self.pixels, self.size)
    }

    /// The canvas without copying: the same bytes and size as [`TileDecoder::canvas`].
    /// Copy-on-write: the next `apply` copies the canvas only if a returned `Arc` is still alive.
    pub fn shared_canvas(&self) -> (Arc<[u8]>, PixelSize) {
        (Arc::clone(&self.pixels), self.size)
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
        Self::for_size(PixelSize::new(header.width, header.height), tx, ty)
    }

    fn for_size(size: PixelSize, tx: u32, ty: u32) -> Self {
        let x = tx * TILE;
        let y = ty * TILE;
        Self {
            x,
            y,
            width: TILE.min(size.width - x),
            height: TILE.min(size.height - y),
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

fn validate_pixels(size: PixelSize, pixels: &[u8], stride: u32) -> Result<(), MediaError> {
    let row_bytes = size.width as usize * 4;
    let stride = stride as usize;
    if stride < row_bytes {
        return Err(MediaError::BadPayload);
    }
    let required = (size.height as usize - 1)
        .checked_mul(stride)
        .and_then(|offset| offset.checked_add(row_bytes))
        .ok_or(MediaError::BadPayload)?;
    if pixels.len() < required {
        return Err(MediaError::BadPayload);
    }
    Ok(())
}
