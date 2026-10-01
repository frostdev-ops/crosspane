//! E2 v0 media payloads: a little-endian CPF1 header followed by lossless tile records.

pub const MAGIC: u32 = 0x3146_5043;
pub const TILE: u32 = 64;
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

pub(crate) const HEADER_BYTES: usize = 48;
pub(crate) const RECORD_BYTES: usize = 12;
const MAX_DIMENSION: u32 = 16384;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    pub projection: u64,
    pub seq: u64,
    pub key: bool,
    pub captured_ns: u64,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MediaError {
    #[error("truncated media frame")]
    Truncated,
    #[error("bad media magic")]
    BadMagic,
    #[error("unsupported media version")]
    BadVersion,
    #[error("unsupported media codec")]
    BadCodec,
    #[error("nonzero reserved field or flag")]
    BadReserved,
    #[error("invalid frame dimensions")]
    BadSize,
    #[error("invalid tile size, index, or count")]
    BadTile,
    #[error("duplicate tile")]
    Duplicate,
    #[error("invalid tile encoding or payload")]
    BadPayload,
    #[error("media frame exceeds the byte limit or allocation capacity")]
    TooLarge,
    #[error("key frame is missing tiles")]
    MissingTiles,
    #[error("delta received before a key frame")]
    NoCanvas,
    #[error("delta dimensions differ from the canvas")]
    SizeMismatch,
    #[error("trailing media frame bytes")]
    Trailing,
}

/// What a media frame carries (header byte 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// 0: lossless tile records (`tiles`).
    Tiles,
    /// 1: one H.264 Annex B access unit (WP-2.14). The header's tile-size field is 0 and its
    /// count field is the access unit's length in bytes; `key` marks an IDR.
    H264,
}

/// The codec of a media frame, from its header alone.
pub fn read_codec(data: &[u8]) -> Result<Codec, MediaError> {
    let mut input = Reader::new(data);
    if input.u32()? != MAGIC {
        return Err(MediaError::BadMagic);
    }
    if input.u8()? != 1 {
        return Err(MediaError::BadVersion);
    }
    input.u8()?; // Flags are validated by the full header parser.
    parse_codec(input.u8()?)
}

/// Append a codec-1 frame: the CPF1 header for `header` followed by `access_unit`. `out` is
/// cleared first. Fails with `TooLarge` past `MAX_FRAME_BYTES` and `BadSize` for dimensions the
/// tile format would also refuse.
pub fn write_video(
    header: FrameHeader,
    access_unit: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), MediaError> {
    out.clear();
    tile_grid(header.width, header.height)?;
    let len = HEADER_BYTES
        .checked_add(access_unit.len())
        .filter(|&len| len <= MAX_FRAME_BYTES)
        .ok_or(MediaError::TooLarge)?;
    let count = u32::try_from(access_unit.len()).map_err(|_| MediaError::TooLarge)?;
    out.try_reserve_exact(len)
        .map_err(|_| MediaError::TooLarge)?;
    write_codec_header(header, count, Codec::H264, out);
    out.extend_from_slice(access_unit);
    Ok(())
}

/// Parse a codec-1 frame: its header and access unit (borrowed from `data`). Every check the
/// tile format applies to the header applies here too; the payload must be exactly `count` bytes.
pub fn read_video(data: &[u8]) -> Result<(FrameHeader, &[u8]), MediaError> {
    if read_codec(data)? != Codec::H264 {
        return Err(MediaError::BadCodec);
    }
    let (header, count) = parse_header(data)?;
    let mut input = Reader::new(data.get(HEADER_BYTES..).ok_or(MediaError::Truncated)?);
    let access_unit = input.take(count as usize)?;
    if !input.remaining().is_empty() {
        return Err(MediaError::Trailing);
    }
    Ok((header, access_unit))
}

/// Parse just the header (the receiver uses it to drop stale frames before decoding).
/// Tile records are deliberately not inspected here.
pub fn read_header(data: &[u8]) -> Result<FrameHeader, MediaError> {
    parse_header(data).map(|(header, _)| header)
}

pub(crate) fn tile_grid(width: u32, height: u32) -> Result<(u32, u32), MediaError> {
    if !(1..=MAX_DIMENSION).contains(&width) || !(1..=MAX_DIMENSION).contains(&height) {
        return Err(MediaError::BadSize);
    }
    Ok((width.div_ceil(TILE), height.div_ceil(TILE)))
}

pub(crate) fn parse_header(data: &[u8]) -> Result<(FrameHeader, u32), MediaError> {
    if data.len() > MAX_FRAME_BYTES {
        return Err(MediaError::TooLarge);
    }
    let mut input = Reader::new(data.get(..HEADER_BYTES).ok_or(MediaError::Truncated)?);
    if input.u32()? != MAGIC {
        return Err(MediaError::BadMagic);
    }
    if input.u8()? != 1 {
        return Err(MediaError::BadVersion);
    }
    let flags = input.u8()?;
    if flags & !1 != 0 {
        return Err(MediaError::BadReserved);
    }
    let codec = parse_codec(input.u8()?)?;
    if input.u8()? != 0 {
        return Err(MediaError::BadReserved);
    }
    let projection = input.u64()?;
    let seq = input.u64()?;
    let captured_ns = input.u64()?;
    let width = input.u32()?;
    let height = input.u32()?;
    let (tiles_x, tiles_y) = tile_grid(width, height)?;
    let tile_size = u32::from(input.u16()?);
    match codec {
        Codec::Tiles if tile_size != TILE => return Err(MediaError::BadTile),
        // A tile header relabelled as video is still an unsupported codec/format pairing.
        Codec::H264 if tile_size != 0 => return Err(MediaError::BadCodec),
        _ => {}
    }
    if input.u16()? != 0 {
        return Err(MediaError::BadReserved);
    }
    let count = input.u32()?;
    if codec == Codec::Tiles && count > tiles_x * tiles_y {
        return Err(MediaError::BadTile);
    }
    let key = flags & 1 != 0;
    if codec == Codec::Tiles && key && count != tiles_x * tiles_y {
        return Err(MediaError::MissingTiles);
    }
    Ok((
        FrameHeader {
            projection,
            seq,
            key,
            captured_ns,
            width,
            height,
        },
        count,
    ))
}

pub(crate) fn write_header(header: FrameHeader, count: u32, out: &mut Vec<u8>) {
    write_codec_header(header, count, Codec::Tiles, out);
}

fn parse_codec(byte: u8) -> Result<Codec, MediaError> {
    match byte {
        0 => Ok(Codec::Tiles),
        1 => Ok(Codec::H264),
        _ => Err(MediaError::BadCodec),
    }
}

fn write_codec_header(header: FrameHeader, count: u32, codec: Codec, out: &mut Vec<u8>) {
    let (codec_byte, tile_size) = match codec {
        Codec::Tiles => (0, TILE as u16),
        Codec::H264 => (1, 0),
    };
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&[1, u8::from(header.key), codec_byte, 0]);
    out.extend_from_slice(&header.projection.to_le_bytes());
    out.extend_from_slice(&header.seq.to_le_bytes());
    out.extend_from_slice(&header.captured_ns.to_le_bytes());
    out.extend_from_slice(&header.width.to_le_bytes());
    out.extend_from_slice(&header.height.to_le_bytes());
    out.extend_from_slice(&tile_size.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
}

/// All accesses to untrusted wire bytes are checked before use.
pub(crate) struct Reader<'a> {
    remaining: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { remaining: data }
    }

    pub(crate) fn remaining(&self) -> &'a [u8] {
        self.remaining
    }

    pub(crate) fn take(&mut self, len: usize) -> Result<&'a [u8], MediaError> {
        let bytes = self.remaining.get(..len).ok_or(MediaError::Truncated)?;
        self.remaining = self.remaining.get(len..).ok_or(MediaError::Truncated)?;
        Ok(bytes)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], MediaError> {
        self.take(N)?.try_into().map_err(|_| MediaError::Truncated)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, MediaError> {
        Ok(u8::from_le_bytes(self.array()?))
    }

    pub(crate) fn u16(&mut self) -> Result<u16, MediaError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, MediaError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, MediaError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
}
