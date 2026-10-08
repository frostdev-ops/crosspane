//! CPD1, the wire codec for the IddCx twin control interface (`drivers/windows-idd`,
//! `include/crosspane_idd_v1.h`). Pure and little-endian: no OS types and no `unsafe`.
//!
//! Every request and reply is a fixed-size frame. A 32-byte header (magic, version, struct size,
//! flags, request id, reserved) comes first, then the body. Encoders refuse zero ids and invalid
//! modes. Decoders refuse every frame the driver could not have produced, and never panic on any
//! input. The offsets below are the header's `offsetof` values; the codec tests check them.

/// Frame magic, `"CPD1"` in little-endian byte order.
pub const MAGIC: u32 = 0x3144_5043;
/// Protocol major version.
pub const MAJOR: u16 = 1;
/// Protocol minor version.
pub const MINOR: u16 = 0;
/// `CTL_CODE(0x8337, 0x800)`: buffered, read and write access.
pub const IOCTL_ADD: u32 = 0x8337_e000;
/// `CTL_CODE(0x8337, 0x801)`: buffered, read and write access.
pub const IOCTL_REMOVE: u32 = 0x8337_e004;
/// `CTL_CODE(0x8337, 0x802)`: buffered, read access.
pub const IOCTL_LIST: u32 = 0x8337_6008;
/// `CTL_CODE(0x8337, 0x803)`: buffered, read and write access.
pub const IOCTL_HEARTBEAT: u32 = 0x8337_e00c;
/// Monitor state: live.
pub const MONITOR_ACTIVE: u32 = 1;
/// Monitor state: retired by a REMOVE.
pub const MONITOR_RETIRED: u32 = 2;
/// Opens the driver accepts at once.
pub const MAX_OPENS: usize = 4;
/// Monitors the driver holds at once.
pub const MAX_MONITORS: usize = 4;
/// Monitors one open handle may own.
pub const MONITORS_PER_OPEN: u32 = 1;
/// Lease length after each heartbeat, in milliseconds.
pub const LEASE_MS: u32 = 5_000;
/// Heartbeat cadence, in milliseconds.
pub const HEARTBEAT_INTERVAL_MS: u32 = 1_000;
/// Driver lease-expiry scan period, in milliseconds.
pub const EXPIRY_SCAN_MS: u32 = 100;
/// ADD request frame length.
pub const ADD_REQUEST_BYTES: usize = 64;
/// ADD reply frame length.
pub const ADD_RESPONSE_BYTES: usize = 48;
/// REMOVE request frame length.
pub const REMOVE_REQUEST_BYTES: usize = 48;
/// REMOVE reply frame length.
pub const REMOVE_RESPONSE_BYTES: usize = 48;
/// LIST request frame length.
pub const LIST_REQUEST_BYTES: usize = 32;
/// LIST reply frame length.
pub const LIST_RESPONSE_BYTES: usize = 96;
/// HEARTBEAT request frame length.
pub const HEARTBEAT_REQUEST_BYTES: usize = 48;
/// HEARTBEAT reply frame length.
pub const HEARTBEAT_RESPONSE_BYTES: usize = 48;
/// Device interface GUID of the control interface.
pub const CONTROL_INTERFACE_GUID: u128 = 0xfea027fb_8535_40eb_b887_1401d63ef273;
/// Hardware ID of the twin driver's device node.
pub const HARDWARE_ID: &str = r"Crosspane\IddTwinV1";

/// A display mode for a twin: pixels and physical size in millimetres.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CpdMode {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Physical width in millimetres.
    pub width_mm: u32,
    /// Physical height in millimetres.
    pub height_mm: u32,
}

/// Why a frame was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpdError {
    /// A mode outside the accepted set, or a mode field the driver would refuse.
    Mode,
    /// A zero request id, sequence or monitor id.
    ZeroId,
    /// Wrong frame length.
    Size,
    /// A header field differs from the protocol: magic, version, struct size, flags or reserved.
    Header,
    /// The reply does not echo the request: request id, monitor id or sequence.
    Echo,
    /// A body field is out of range or in the wrong state.
    Field,
}

/// A decoded ADD reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AddReply {
    /// Id of the new monitor. Never zero.
    pub monitor_id: u32,
    /// Lease time left, at most [`LEASE_MS`].
    pub remaining_ms: u32,
}

/// A decoded LIST reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListReply {
    /// Lease time left, at most [`LEASE_MS`].
    pub remaining_ms: u32,
    /// The handle's one monitor, when it has one.
    pub monitor: Option<(u32, CpdMode)>,
}

/// A decoded HEARTBEAT reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeartbeatReply {
    /// Lease time left, at most [`LEASE_MS`].
    pub remaining_ms: u32,
    /// Monitors the handle owns, at most [`MONITORS_PER_OPEN`].
    pub active_count: u32,
}

/// Accepted resolutions. Each is paired with 60/1 Hz and 32 bits per pixel.
const RESOLUTIONS: [(u32, u32); 2] = [(1280, 720), (1920, 1080)];
/// Physical size bounds in millimetres, inclusive.
const MM_MIN: u32 = 10;
const MM_MAX: u32 = 2_000;
const REFRESH_NUMERATOR: u32 = 60;
const REFRESH_DENOMINATOR: u32 = 1;
const BITS_PER_PIXEL: u32 = 32;

// Header (`CPD_HEADER`, 32 bytes).
const H_MAGIC: usize = 0;
const H_MAJOR: usize = 4;
const H_MINOR: usize = 6;
const H_STRUCT_BYTES: usize = 8;
const H_FLAGS: usize = 12;
const H_REQUEST_ID: usize = 16;
const H_RESERVED: usize = 24;

// `CPD_MODE` (32 bytes), as it sits in the ADD request at 32.
const M_WIDTH: usize = 0;
const M_HEIGHT: usize = 4;
const M_REFRESH_NUMERATOR: usize = 8;
const M_REFRESH_DENOMINATOR: usize = 12;
const M_WIDTH_MM: usize = 16;
const M_HEIGHT_MM: usize = 20;
const M_BITS_PER_PIXEL: usize = 24;
const M_RESERVED: usize = 28;

// Bodies, after the header.
const BODY: usize = 32;
// ADD request: the mode at 32.
// ADD reply: monitor_id u64 @32, remaining_ms @40, reserved @44.
const ADD_ID: usize = 32;
const ADD_REMAINING: usize = 40;
const ADD_RESERVED: usize = 44;
// REMOVE request: monitor_id u64 @32, reserved u64 @40.
// REMOVE reply: monitor_id u64 @32, state @40, reserved @44.
const REMOVE_ID: usize = 32;
const REMOVE_STATE: usize = 40;
const REMOVE_RESERVED: usize = 44;
// LIST reply: count @32, capacity @36, remaining_ms @40, reserved @44, entry @48.
const LIST_COUNT: usize = 32;
const LIST_CAPACITY: usize = 36;
const LIST_REMAINING: usize = 40;
const LIST_RESERVED: usize = 44;
const LIST_ENTRY: usize = 48;
// The entry (`CPD_LIST_ENTRY`, 48 bytes): monitor_id u64 +0, mode +8, state +40, reserved +44.
const E_ID: usize = 0;
const E_MODE: usize = 8;
const E_STATE: usize = 40;
const E_RESERVED: usize = 44;
// HEARTBEAT request: sequence u64 @32, reserved u64 @40.
const HB_SEQUENCE: usize = 32;
const HB_REQUEST_RESERVED: usize = 40;
// HEARTBEAT reply: remaining_ms @32, active_count @36, accepted_sequence u64 @40.
const HB_REMAINING: usize = 32;
const HB_ACTIVE: usize = 36;
const HB_ACCEPTED: usize = 40;

/// Whether a twin mode is one the driver accepts.
pub fn valid_mode(mode: CpdMode) -> bool {
    RESOLUTIONS.contains(&(mode.width, mode.height))
        && (MM_MIN..=MM_MAX).contains(&mode.width_mm)
        && (MM_MIN..=MM_MAX).contains(&mode.height_mm)
}

/// Encodes an ADD request for one monitor in `mode`.
pub fn encode_add(request_id: u64, mode: CpdMode) -> Result<[u8; ADD_REQUEST_BYTES], CpdError> {
    if request_id == 0 {
        return Err(CpdError::ZeroId);
    }
    if !valid_mode(mode) {
        return Err(CpdError::Mode);
    }
    let mut frame = [0_u8; ADD_REQUEST_BYTES];
    write_header(&mut frame, request_id);
    write_mode(&mut frame, BODY, mode);
    Ok(frame)
}

/// Encodes a REMOVE request for `monitor_id`.
pub fn encode_remove(
    request_id: u64,
    monitor_id: u32,
) -> Result<[u8; REMOVE_REQUEST_BYTES], CpdError> {
    if request_id == 0 || monitor_id == 0 {
        return Err(CpdError::ZeroId);
    }
    let mut frame = [0_u8; REMOVE_REQUEST_BYTES];
    write_header(&mut frame, request_id);
    put(&mut frame, REMOVE_ID, &u64::from(monitor_id).to_le_bytes());
    Ok(frame)
}

/// Encodes a LIST request.
pub fn encode_list(request_id: u64) -> Result<[u8; LIST_REQUEST_BYTES], CpdError> {
    if request_id == 0 {
        return Err(CpdError::ZeroId);
    }
    let mut frame = [0_u8; LIST_REQUEST_BYTES];
    write_header(&mut frame, request_id);
    Ok(frame)
}

/// Encodes a HEARTBEAT request carrying `sequence`.
pub fn encode_heartbeat(
    request_id: u64,
    sequence: u64,
) -> Result<[u8; HEARTBEAT_REQUEST_BYTES], CpdError> {
    if request_id == 0 || sequence == 0 {
        return Err(CpdError::ZeroId);
    }
    let mut frame = [0_u8; HEARTBEAT_REQUEST_BYTES];
    write_header(&mut frame, request_id);
    put(&mut frame, HB_SEQUENCE, &sequence.to_le_bytes());
    put(&mut frame, HB_REQUEST_RESERVED, &0_u64.to_le_bytes());
    Ok(frame)
}

/// Decodes an ADD reply to `request_id`.
pub fn decode_add(request_id: u64, reply: &[u8]) -> Result<AddReply, CpdError> {
    if request_id == 0 {
        return Err(CpdError::ZeroId);
    }
    check_reply(reply, ADD_RESPONSE_BYTES, request_id)?;
    let monitor_id = nonzero_monitor_id(u64_at(reply, ADD_ID)?)?;
    let remaining_ms = remaining_at(reply, ADD_REMAINING)?;
    if u32_at(reply, ADD_RESERVED)? != 0 {
        return Err(CpdError::Field);
    }
    Ok(AddReply {
        monitor_id,
        remaining_ms,
    })
}

/// Decodes a REMOVE reply to `request_id`, which removed `monitor_id`.
pub fn decode_remove(request_id: u64, monitor_id: u32, reply: &[u8]) -> Result<(), CpdError> {
    if request_id == 0 || monitor_id == 0 {
        return Err(CpdError::ZeroId);
    }
    check_reply(reply, REMOVE_RESPONSE_BYTES, request_id)?;
    if u64_at(reply, REMOVE_ID)? != u64::from(monitor_id) {
        return Err(CpdError::Echo);
    }
    if u32_at(reply, REMOVE_STATE)? != MONITOR_RETIRED {
        return Err(CpdError::Field);
    }
    if u32_at(reply, REMOVE_RESERVED)? != 0 {
        return Err(CpdError::Field);
    }
    Ok(())
}

/// Decodes a LIST reply to `request_id`.
pub fn decode_list(request_id: u64, reply: &[u8]) -> Result<ListReply, CpdError> {
    if request_id == 0 {
        return Err(CpdError::ZeroId);
    }
    check_reply(reply, LIST_RESPONSE_BYTES, request_id)?;
    let count = u32_at(reply, LIST_COUNT)?;
    let capacity = u32_at(reply, LIST_CAPACITY)?;
    let remaining_ms = remaining_at(reply, LIST_REMAINING)?;
    if capacity != MONITORS_PER_OPEN || u32_at(reply, LIST_RESERVED)? != 0 {
        return Err(CpdError::Field);
    }
    let monitor = match count {
        0 => {
            if !is_zero(reply, LIST_ENTRY, LIST_RESPONSE_BYTES)? {
                return Err(CpdError::Field);
            }
            None
        }
        1 => Some(decode_entry(reply)?),
        _ => return Err(CpdError::Field),
    };
    Ok(ListReply {
        remaining_ms,
        monitor,
    })
}

/// Decodes the one live entry of a LIST reply whose count is 1.
fn decode_entry(reply: &[u8]) -> Result<(u32, CpdMode), CpdError> {
    let monitor_id = nonzero_monitor_id(u64_at(reply, LIST_ENTRY + E_ID)?)?;
    let at = LIST_ENTRY + E_MODE;
    let mode = CpdMode {
        width: u32_at(reply, at + M_WIDTH)?,
        height: u32_at(reply, at + M_HEIGHT)?,
        width_mm: u32_at(reply, at + M_WIDTH_MM)?,
        height_mm: u32_at(reply, at + M_HEIGHT_MM)?,
    };
    if u32_at(reply, at + M_REFRESH_NUMERATOR)? != REFRESH_NUMERATOR
        || u32_at(reply, at + M_REFRESH_DENOMINATOR)? != REFRESH_DENOMINATOR
        || u32_at(reply, at + M_BITS_PER_PIXEL)? != BITS_PER_PIXEL
        || u32_at(reply, at + M_RESERVED)? != 0
        || !valid_mode(mode)
    {
        return Err(CpdError::Mode);
    }
    if u32_at(reply, LIST_ENTRY + E_STATE)? != MONITOR_ACTIVE
        || u32_at(reply, LIST_ENTRY + E_RESERVED)? != 0
    {
        return Err(CpdError::Field);
    }
    Ok((monitor_id, mode))
}

/// Decodes a HEARTBEAT reply to `request_id`, which answers `sequence`.
pub fn decode_heartbeat(
    request_id: u64,
    sequence: u64,
    reply: &[u8],
) -> Result<HeartbeatReply, CpdError> {
    if request_id == 0 || sequence == 0 {
        return Err(CpdError::ZeroId);
    }
    check_reply(reply, HEARTBEAT_RESPONSE_BYTES, request_id)?;
    let remaining_ms = remaining_at(reply, HB_REMAINING)?;
    let active_count = u32_at(reply, HB_ACTIVE)?;
    if u64_at(reply, HB_ACCEPTED)? != sequence {
        return Err(CpdError::Echo);
    }
    if active_count > MONITORS_PER_OPEN {
        return Err(CpdError::Field);
    }
    Ok(HeartbeatReply {
        remaining_ms,
        active_count,
    })
}

/// Writes `bytes` at `at`. Callers pass the layout constants above, which all fit their frame.
fn put(frame: &mut [u8], at: usize, bytes: &[u8]) {
    frame[at..at + bytes.len()].copy_from_slice(bytes);
}

/// Writes the header of a request whose frame is `frame`. The struct size is the frame length.
fn write_header(frame: &mut [u8], request_id: u64) {
    let struct_bytes = frame.len() as u32;
    put(frame, H_MAGIC, &MAGIC.to_le_bytes());
    put(frame, H_MAJOR, &MAJOR.to_le_bytes());
    put(frame, H_MINOR, &MINOR.to_le_bytes());
    put(frame, H_STRUCT_BYTES, &struct_bytes.to_le_bytes());
    put(frame, H_FLAGS, &0_u32.to_le_bytes());
    put(frame, H_REQUEST_ID, &request_id.to_le_bytes());
    put(frame, H_RESERVED, &0_u64.to_le_bytes());
}

/// Writes a mode at `at`. The frame starts zeroed, so the reserved field stays zero.
fn write_mode(frame: &mut [u8], at: usize, mode: CpdMode) {
    put(frame, at + M_WIDTH, &mode.width.to_le_bytes());
    put(frame, at + M_HEIGHT, &mode.height.to_le_bytes());
    put(
        frame,
        at + M_REFRESH_NUMERATOR,
        &REFRESH_NUMERATOR.to_le_bytes(),
    );
    put(
        frame,
        at + M_REFRESH_DENOMINATOR,
        &REFRESH_DENOMINATOR.to_le_bytes(),
    );
    put(frame, at + M_WIDTH_MM, &mode.width_mm.to_le_bytes());
    put(frame, at + M_HEIGHT_MM, &mode.height_mm.to_le_bytes());
    put(frame, at + M_BITS_PER_PIXEL, &BITS_PER_PIXEL.to_le_bytes());
}

/// Reads `N` bytes at `at`, or `Size` when the reply is too short for the field.
fn raw<const N: usize>(reply: &[u8], at: usize) -> Result<[u8; N], CpdError> {
    reply
        .get(at..at + N)
        .and_then(|bytes| <[u8; N]>::try_from(bytes).ok())
        .ok_or(CpdError::Size)
}

fn u32_at(reply: &[u8], at: usize) -> Result<u32, CpdError> {
    Ok(u32::from_le_bytes(raw(reply, at)?))
}

fn u64_at(reply: &[u8], at: usize) -> Result<u64, CpdError> {
    Ok(u64::from_le_bytes(raw(reply, at)?))
}

fn u16_at(reply: &[u8], at: usize) -> Result<u16, CpdError> {
    Ok(u16::from_le_bytes(raw(reply, at)?))
}

/// Whether `reply[from..to]` is all zero.
fn is_zero(reply: &[u8], from: usize, to: usize) -> Result<bool, CpdError> {
    reply
        .get(from..to)
        .map(|bytes| bytes.iter().all(|byte| *byte == 0))
        .ok_or(CpdError::Size)
}

/// A lease time, which may not exceed [`LEASE_MS`].
fn remaining_at(reply: &[u8], at: usize) -> Result<u32, CpdError> {
    let remaining_ms = u32_at(reply, at)?;
    if remaining_ms > LEASE_MS {
        return Err(CpdError::Field);
    }
    Ok(remaining_ms)
}

/// A monitor id that is nonzero and fits in `u32`.
fn nonzero_monitor_id(value: u64) -> Result<u32, CpdError> {
    match value {
        0 => Err(CpdError::ZeroId),
        _ => u32::try_from(value).map_err(|_| CpdError::Field),
    }
}

/// Checks length, then the header, then the echoed request id. The reply length is `expected`.
fn check_reply(reply: &[u8], expected: usize, request_id: u64) -> Result<(), CpdError> {
    if reply.len() != expected {
        return Err(CpdError::Size);
    }
    let header_ok = u32_at(reply, H_MAGIC)? == MAGIC
        && u16_at(reply, H_MAJOR)? == MAJOR
        && u16_at(reply, H_MINOR)? == MINOR
        && usize::try_from(u32_at(reply, H_STRUCT_BYTES)?).ok() == Some(expected)
        && u32_at(reply, H_FLAGS)? == 0
        && u64_at(reply, H_RESERVED)? == 0;
    if !header_ok {
        return Err(CpdError::Header);
    }
    if u64_at(reply, H_REQUEST_ID)? != request_id {
        return Err(CpdError::Echo);
    }
    Ok(())
}
