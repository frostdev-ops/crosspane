//! Hot-path encodings: input-stream messages and pointer datagrams. Implemented in WP-1.1.

use crosspane_types::geom::{PixelSize, PointDevice, VectorLogical};
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::{DisplayId, ProjectionId, SessionId, WindowId};
use crosspane_types::input::{LockKeys, ScrollDelta, ScrollPhase};

use super::frame::decode_header;
use super::{
    Frame, HEADER_LEN, KIND_ACK, KIND_BUTTON, KIND_KEY, KIND_LOCK_KEYS, KIND_POINTER,
    KIND_PRESS_AT, KIND_PROJ_BUTTON, KIND_PROJ_HELD, KIND_PROJ_KEY, KIND_PROJ_MOTION,
    KIND_PROJ_SCROLL, KIND_SCROLL, KIND_STATE, KIND_STATUS, KIND_STATUS_MOVE, MAX_INPUT_PAYLOAD,
    WIRE_VERSION, WireError,
};
use crate::msg::{InputMessage, MAX_HELD_KEYS, PointerMessage, Refusal, TargetStatus};
use crate::projection::ProjInput;

/// Append one framed input message to `out`.
pub fn encode_input(msg: &InputMessage, out: &mut Vec<u8>) -> Result<(), WireError> {
    match msg {
        InputMessage::Proj(input) => encode_projection_input(input, out)?,
        InputMessage::PressAt {
            session,
            seq,
            button,
            display,
            position,
        } => {
            check_button(*button)?;
            check_position(*position)?;
            // session u64, seq u32, button u8, display u32, x/y f64 (little endian).
            input_prefix(out, KIND_PRESS_AT, 33, *session, *seq);
            out.push(button.0);
            out.extend_from_slice(&display.0.to_le_bytes());
            append_position(out, *position);
        }
        InputMessage::Key {
            session,
            seq,
            usage,
            down,
        } => {
            input_prefix(out, KIND_KEY, 17, *session, *seq);
            out.extend_from_slice(&usage.page.to_le_bytes());
            out.extend_from_slice(&usage.id.to_le_bytes());
            out.push(u8::from(*down));
        }
        InputMessage::Button {
            session,
            seq,
            button,
            down,
        } => {
            check_button(*button)?;
            input_prefix(out, KIND_BUTTON, 14, *session, *seq);
            out.extend_from_slice(&[button.0, u8::from(*down)]);
        }
        InputMessage::Scroll {
            session,
            seq,
            delta,
        } => {
            let (x, y) = match delta.pixels {
                Some(pixels) => (finite_f32(pixels.x)?, finite_f32(pixels.y)?),
                None => (0.0, 0.0),
            };
            let flags = u8::from(delta.pixels.is_some())
                | (u8::from(delta.stop_x) << 1)
                | (u8::from(delta.stop_y) << 2);
            input_prefix(out, KIND_SCROLL, 30, *session, *seq);
            out.extend_from_slice(&delta.v120_x.to_le_bytes());
            out.extend_from_slice(&delta.v120_y.to_le_bytes());
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
            out.extend_from_slice(&[flags, encode_phase(delta.phase)]);
        }
        InputMessage::LockKeys { session, seq, keys } => {
            input_prefix(out, KIND_LOCK_KEYS, 15, *session, *seq);
            out.extend_from_slice(&[
                encode_lock(keys.caps_lock),
                encode_lock(keys.num_lock),
                encode_lock(keys.scroll_lock),
            ]);
        }
        InputMessage::State {
            session,
            seq,
            held_keys,
            held_buttons,
        } => {
            if held_keys.len() > MAX_HELD_KEYS {
                return Err(WireError::BadValue("too many held keys"));
            }
            let mut buttons = 0u16;
            for button in held_buttons {
                if !(1..=16).contains(&button.0) {
                    return Err(WireError::BadValue("held button outside 1..=16"));
                }
                buttons |= 1 << (button.0 - 1);
            }
            input_prefix(
                out,
                KIND_STATE,
                15 + 4 * held_keys.len() as u32,
                *session,
                *seq,
            );
            out.extend_from_slice(&buttons.to_le_bytes());
            out.push(held_keys.len() as u8);
            for usage in held_keys {
                out.extend_from_slice(&usage.page.to_le_bytes());
                out.extend_from_slice(&usage.id.to_le_bytes());
            }
        }
        InputMessage::Ack { session, seq } => {
            input_prefix(out, KIND_ACK, 12, *session, *seq);
        }
        InputMessage::Status { session, status } => {
            let codes = match status {
                TargetStatus::NativeMove { .. } | TargetStatus::NativeMoveEnded { .. } => {
                    encode_move_status(*session, *status, out);
                    return Ok(());
                }
                TargetStatus::LocalOverride => [1, 0],
                TargetStatus::Resumed => [2, 0],
                TargetStatus::Refused(reason) => [
                    3,
                    match reason {
                        Refusal::Permission => 1,
                        Refusal::Locked => 2,
                        Refusal::SecureInput => 3,
                        Refusal::Busy => 4,
                        Refusal::InjectorFailed => 5,
                    },
                ],
            };
            append_header(out, KIND_STATUS, 10);
            out.extend_from_slice(&session.0.to_le_bytes());
            out.extend_from_slice(&codes);
        }
    }
    Ok(())
}

/// Decode an input-stream frame.
pub fn decode_input(frame: &Frame) -> Result<InputMessage, WireError> {
    if (KIND_PROJ_KEY..=KIND_PROJ_HELD).contains(&frame.kind) {
        return decode_projection_input(frame).map(InputMessage::Proj);
    }
    let len = frame.payload.len();
    let expected = match frame.kind {
        KIND_KEY => 17,
        KIND_BUTTON => 14,
        KIND_PRESS_AT => 33,
        KIND_SCROLL => 30,
        KIND_LOCK_KEYS => 15,
        KIND_STATE if len >= 15 => len,
        KIND_STATE => 15,
        KIND_ACK => 12,
        KIND_STATUS => 10,
        KIND_STATUS_MOVE => 42,
        kind => return Err(WireError::BadKind(kind)),
    };
    if len != expected {
        return Err(WireError::BadLength {
            kind: frame.kind,
            len,
        });
    }
    let mut reader = Reader(&frame.payload);
    let session = SessionId(u64::from_le_bytes(reader.take()?));
    if frame.kind == KIND_STATUS {
        return Ok(InputMessage::Status {
            session,
            status: decode_status(reader.byte()?, reader.byte()?)?,
        });
    }
    if frame.kind == KIND_STATUS_MOVE {
        return Ok(InputMessage::Status {
            session,
            status: decode_move_status(&mut reader)?,
        });
    }
    let seq = u32::from_le_bytes(reader.take()?);
    match frame.kind {
        KIND_PRESS_AT => {
            let button = MouseButton(reader.byte()?);
            check_button(button)?;
            Ok(InputMessage::PressAt {
                session,
                seq,
                button,
                display: DisplayId(u32::from_le_bytes(reader.take()?)),
                position: reader.position()?,
            })
        }
        KIND_KEY => Ok(InputMessage::Key {
            session,
            seq,
            usage: reader.usage()?,
            down: decode_down(reader.byte()?)?,
        }),
        KIND_BUTTON => {
            let button = MouseButton(reader.byte()?);
            check_button(button)?;
            Ok(InputMessage::Button {
                session,
                seq,
                button,
                down: decode_down(reader.byte()?)?,
            })
        }
        KIND_SCROLL => {
            let v120_x = i32::from_le_bytes(reader.take()?);
            let v120_y = i32::from_le_bytes(reader.take()?);
            let x = f32::from_le_bytes(reader.take()?);
            let y = f32::from_le_bytes(reader.take()?);
            let flags = reader.byte()?;
            if flags & !7 != 0 {
                return Err(WireError::BadValue("unknown scroll flags"));
            }
            let phase = decode_phase(reader.byte()?)?;
            let pixels = if flags & 1 != 0 {
                Some(VectorLogical::new(finite_f64(x)?, finite_f64(y)?))
            } else {
                // Absent pixel fields have no meaning, regardless of their bit patterns.
                None
            };
            Ok(InputMessage::Scroll {
                session,
                seq,
                delta: ScrollDelta {
                    v120_x,
                    v120_y,
                    pixels,
                    phase,
                    stop_x: flags & 2 != 0,
                    stop_y: flags & 4 != 0,
                },
            })
        }
        KIND_LOCK_KEYS => Ok(InputMessage::LockKeys {
            session,
            seq,
            keys: LockKeys {
                caps_lock: decode_lock(reader.byte()?)?,
                num_lock: decode_lock(reader.byte()?)?,
                scroll_lock: decode_lock(reader.byte()?)?,
            },
        }),
        KIND_STATE => {
            let buttons = u16::from_le_bytes(reader.take()?);
            let count = usize::from(reader.byte()?);
            if count > MAX_HELD_KEYS {
                return Err(WireError::BadValue("too many held keys"));
            }
            if len != 15 + 4 * count {
                return Err(WireError::BadLength {
                    kind: KIND_STATE,
                    len,
                });
            }
            let mut held_keys = Vec::with_capacity(count);
            for _ in 0..count {
                held_keys.push(reader.usage()?);
            }
            let held_buttons = (1..=16)
                .filter(|button| buttons & (1 << (button - 1)) != 0)
                .map(MouseButton)
                .collect();
            Ok(InputMessage::State {
                session,
                seq,
                held_keys,
                held_buttons,
            })
        }
        KIND_ACK => Ok(InputMessage::Ack { session, seq }),
        kind => Err(WireError::BadKind(kind)),
    }
}

// E2 payloads are packed, little-endian, with no padding. Offsets exclude the frame header.
// All start with projection u64 at 0 and seq u32 at 8.
// Key (17): page u16 at 12, id u16 at 14, down u8 at 16.
// Button (30): button/down u8 at 12/13, position x/y f64 at 14/22.
// Scroll (46): E1 delta at 12..30, position x/y f64 at 30/38.
// Motion (28): position x/y f64 at 12/20.
// Held (14 + 4*K + B): key/button counts u8 at 12/13, K usages at 14, then B buttons.
const MAX_PROJ_HELD_BUTTONS: usize = 16;

fn encode_projection_input(input: &ProjInput, out: &mut Vec<u8>) -> Result<(), WireError> {
    match input {
        ProjInput::Key {
            projection,
            seq,
            usage,
            down,
        } => {
            projection_prefix(out, KIND_PROJ_KEY, 17, *projection, *seq);
            out.extend_from_slice(&usage.page.to_le_bytes());
            out.extend_from_slice(&usage.id.to_le_bytes());
            out.push(u8::from(*down));
        }
        ProjInput::Button {
            projection,
            seq,
            button,
            down,
            position,
        } => {
            check_button(*button)?;
            check_position(*position)?;
            projection_prefix(out, KIND_PROJ_BUTTON, 30, *projection, *seq);
            out.extend_from_slice(&[button.0, u8::from(*down)]);
            append_position(out, *position);
        }
        ProjInput::Scroll {
            projection,
            seq,
            delta,
            position,
        } => {
            check_position(*position)?;
            let (x, y) = match delta.pixels {
                Some(pixels) => (finite_f32(pixels.x)?, finite_f32(pixels.y)?),
                None => (0.0, 0.0),
            };
            let flags = u8::from(delta.pixels.is_some())
                | (u8::from(delta.stop_x) << 1)
                | (u8::from(delta.stop_y) << 2);
            projection_prefix(out, KIND_PROJ_SCROLL, 46, *projection, *seq);
            out.extend_from_slice(&delta.v120_x.to_le_bytes());
            out.extend_from_slice(&delta.v120_y.to_le_bytes());
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
            out.extend_from_slice(&[flags, encode_phase(delta.phase)]);
            append_position(out, *position);
        }
        ProjInput::Motion {
            projection,
            seq,
            position,
        } => {
            check_position(*position)?;
            projection_prefix(out, KIND_PROJ_MOTION, 28, *projection, *seq);
            append_position(out, *position);
        }
        ProjInput::Held {
            projection,
            seq,
            keys,
            buttons,
        } => {
            check_held_counts(keys.len(), buttons.len())?;
            for button in buttons {
                check_held_button(*button)?;
            }
            let len = 14 + 4 * keys.len() + buttons.len();
            projection_prefix(out, KIND_PROJ_HELD, len as u32, *projection, *seq);
            out.extend_from_slice(&[keys.len() as u8, buttons.len() as u8]);
            for usage in keys {
                out.extend_from_slice(&usage.page.to_le_bytes());
                out.extend_from_slice(&usage.id.to_le_bytes());
            }
            out.extend(buttons.iter().map(|button| button.0));
        }
    }
    Ok(())
}

fn decode_projection_input(frame: &Frame) -> Result<ProjInput, WireError> {
    let kind = frame.kind;
    let len = frame.payload.len();
    let expected = match kind {
        KIND_PROJ_KEY => 17,
        KIND_PROJ_BUTTON => 30,
        KIND_PROJ_SCROLL => 46,
        KIND_PROJ_MOTION => 28,
        KIND_PROJ_HELD if len >= 14 => len,
        KIND_PROJ_HELD => 14,
        _ => return Err(WireError::BadKind(kind)),
    };
    if len != expected {
        return Err(WireError::BadLength { kind, len });
    }
    let mut reader = Reader(&frame.payload);
    let projection = ProjectionId(u64::from_le_bytes(reader.take()?));
    let seq = u32::from_le_bytes(reader.take()?);
    match kind {
        KIND_PROJ_KEY => Ok(ProjInput::Key {
            projection,
            seq,
            usage: reader.usage()?,
            down: decode_down(reader.byte()?)?,
        }),
        KIND_PROJ_BUTTON => {
            let button = MouseButton(reader.byte()?);
            check_button(button)?;
            Ok(ProjInput::Button {
                projection,
                seq,
                button,
                down: decode_down(reader.byte()?)?,
                position: reader.position()?,
            })
        }
        KIND_PROJ_SCROLL => {
            let v120_x = i32::from_le_bytes(reader.take()?);
            let v120_y = i32::from_le_bytes(reader.take()?);
            let x = f32::from_le_bytes(reader.take()?);
            let y = f32::from_le_bytes(reader.take()?);
            let flags = reader.byte()?;
            if flags & !7 != 0 {
                return Err(WireError::BadValue("unknown scroll flags"));
            }
            let phase = decode_phase(reader.byte()?)?;
            // As in E1, absent pixel fields have no meaning regardless of their bit patterns.
            let pixels = if flags & 1 != 0 {
                Some(VectorLogical::new(finite_f64(x)?, finite_f64(y)?))
            } else {
                None
            };
            Ok(ProjInput::Scroll {
                projection,
                seq,
                delta: ScrollDelta {
                    v120_x,
                    v120_y,
                    pixels,
                    phase,
                    stop_x: flags & 2 != 0,
                    stop_y: flags & 4 != 0,
                },
                position: reader.position()?,
            })
        }
        KIND_PROJ_MOTION => Ok(ProjInput::Motion {
            projection,
            seq,
            position: reader.position()?,
        }),
        KIND_PROJ_HELD => {
            let key_count = usize::from(reader.byte()?);
            let button_count = usize::from(reader.byte()?);
            check_held_counts(key_count, button_count)?;
            if len != 14 + 4 * key_count + button_count {
                return Err(WireError::BadLength { kind, len });
            }
            let mut keys = Vec::with_capacity(key_count);
            for _ in 0..key_count {
                keys.push(reader.usage()?);
            }
            let mut buttons = Vec::with_capacity(button_count);
            for _ in 0..button_count {
                let button = MouseButton(reader.byte()?);
                check_held_button(button)?;
                buttons.push(button);
            }
            Ok(ProjInput::Held {
                projection,
                seq,
                keys,
                buttons,
            })
        }
        _ => Err(WireError::BadKind(kind)),
    }
}

fn projection_prefix(out: &mut Vec<u8>, kind: u8, len: u32, projection: ProjectionId, seq: u32) {
    append_header(out, kind, len);
    out.extend_from_slice(&projection.0.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
}

fn check_position(position: PointDevice) -> Result<(), WireError> {
    if position.x.is_finite() && position.y.is_finite() {
        Ok(())
    } else {
        Err(WireError::BadValue("non-finite coordinate"))
    }
}

fn append_position(out: &mut Vec<u8>, position: PointDevice) {
    out.extend_from_slice(&position.x.to_le_bytes());
    out.extend_from_slice(&position.y.to_le_bytes());
}

fn check_held_counts(keys: usize, buttons: usize) -> Result<(), WireError> {
    if keys > MAX_HELD_KEYS {
        return Err(WireError::BadValue("too many held keys"));
    }
    if buttons > MAX_PROJ_HELD_BUTTONS {
        return Err(WireError::BadValue("too many held buttons"));
    }
    Ok(())
}

fn check_held_button(button: MouseButton) -> Result<(), WireError> {
    if !(1..=16).contains(&button.0) {
        return Err(WireError::BadValue("held button outside 1..=16"));
    }
    Ok(())
}

/// Encode a pointer message as one complete datagram (header plus payload).
pub fn encode_pointer(msg: &PointerMessage) -> Result<Vec<u8>, WireError> {
    let x = finite_f32(msg.position.x)?;
    let y = finite_f32(msg.position.y)?;
    let mut out = Vec::with_capacity(HEADER_LEN + 24);
    input_prefix(&mut out, KIND_POINTER, 24, msg.session, msg.seq);
    out.extend_from_slice(&msg.display.0.to_le_bytes());
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
    Ok(out)
}

/// Decode one pointer datagram.
pub fn decode_pointer(datagram: &[u8]) -> Result<PointerMessage, WireError> {
    let (kind, len) = decode_header(datagram, MAX_INPUT_PAYLOAD)?;
    if kind != KIND_POINTER {
        return Err(WireError::BadKind(kind));
    }
    if len != 24 {
        return Err(WireError::BadLength { kind, len });
    }
    let payload = &datagram[HEADER_LEN..];
    if payload.len() < len {
        return Err(WireError::Truncated);
    }
    if payload.len() != len {
        return Err(WireError::BadLength {
            kind,
            len: payload.len(),
        });
    }
    let mut reader = Reader(payload);
    Ok(PointerMessage {
        session: SessionId(u64::from_le_bytes(reader.take()?)),
        seq: u32::from_le_bytes(reader.take()?),
        display: DisplayId(u32::from_le_bytes(reader.take()?)),
        position: PointDevice::new(
            finite_f64(f32::from_le_bytes(reader.take()?))?,
            finite_f64(f32::from_le_bytes(reader.take()?))?,
        ),
    })
}

fn append_header(out: &mut Vec<u8>, kind: u8, len: u32) {
    out.extend_from_slice(&[WIRE_VERSION, kind, 0, 0]);
    out.extend_from_slice(&len.to_le_bytes());
}

fn input_prefix(out: &mut Vec<u8>, kind: u8, len: u32, session: SessionId, seq: u32) {
    append_header(out, kind, len);
    out.extend_from_slice(&session.0.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        let bytes = self.0.get(..N).ok_or(WireError::Truncated)?;
        let mut result = [0; N];
        result.copy_from_slice(bytes);
        self.0 = &self.0[N..];
        Ok(result)
    }

    fn byte(&mut self) -> Result<u8, WireError> {
        Ok(self.take::<1>()?[0])
    }

    fn usage(&mut self) -> Result<HidUsage, WireError> {
        Ok(HidUsage {
            page: u16::from_le_bytes(self.take()?),
            id: u16::from_le_bytes(self.take()?),
        })
    }

    fn position(&mut self) -> Result<PointDevice, WireError> {
        let position = PointDevice::new(
            f64::from_le_bytes(self.take()?),
            f64::from_le_bytes(self.take()?),
        );
        check_position(position)?;
        Ok(position)
    }
}

fn check_button(button: MouseButton) -> Result<(), WireError> {
    if button.0 == 0 {
        Err(WireError::BadValue("button zero"))
    } else {
        Ok(())
    }
}

fn decode_down(code: u8) -> Result<bool, WireError> {
    match code {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(WireError::BadValue("invalid down code")),
    }
}

fn finite_f32(value: f64) -> Result<f32, WireError> {
    let value = value as f32;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(WireError::BadValue("non-finite coordinate"))
    }
}

fn finite_f64(value: f32) -> Result<f64, WireError> {
    if value.is_finite() {
        Ok(f64::from(value))
    } else {
        Err(WireError::BadValue("non-finite coordinate"))
    }
}

fn encode_lock(value: Option<bool>) -> u8 {
    match value {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    }
}

fn decode_lock(code: u8) -> Result<Option<bool>, WireError> {
    match code {
        0 => Ok(None),
        1 => Ok(Some(false)),
        2 => Ok(Some(true)),
        _ => Err(WireError::BadValue("invalid lock-key code")),
    }
}

fn encode_phase(phase: ScrollPhase) -> u8 {
    match phase {
        ScrollPhase::Discrete => 0,
        ScrollPhase::MayBegin => 1,
        ScrollPhase::Began => 2,
        ScrollPhase::Changed => 3,
        ScrollPhase::Ended => 4,
        ScrollPhase::Cancelled => 5,
        ScrollPhase::MomentumBegan => 6,
        ScrollPhase::MomentumChanged => 7,
        ScrollPhase::MomentumEnded => 8,
    }
}

fn decode_phase(code: u8) -> Result<ScrollPhase, WireError> {
    match code {
        0 => Ok(ScrollPhase::Discrete),
        1 => Ok(ScrollPhase::MayBegin),
        2 => Ok(ScrollPhase::Began),
        3 => Ok(ScrollPhase::Changed),
        4 => Ok(ScrollPhase::Ended),
        5 => Ok(ScrollPhase::Cancelled),
        6 => Ok(ScrollPhase::MomentumBegan),
        7 => Ok(ScrollPhase::MomentumChanged),
        8 => Ok(ScrollPhase::MomentumEnded),
        _ => Err(WireError::BadValue("unknown scroll phase")),
    }
}

fn encode_move_status(session: SessionId, status: TargetStatus, out: &mut Vec<u8>) {
    append_header(out, KIND_STATUS_MOVE, 42);
    out.extend_from_slice(&session.0.to_le_bytes());
    match status {
        TargetStatus::NativeMove {
            window,
            proxy,
            grab,
            size,
        } => {
            out.push(1);
            out.extend_from_slice(&window.0.to_le_bytes());
            out.push(u8::from(proxy.is_some()));
            out.extend_from_slice(&proxy.map_or(0, |id| id.0).to_le_bytes());
            out.extend_from_slice(&grab.0.to_le_bytes());
            out.extend_from_slice(&grab.1.to_le_bytes());
            out.extend_from_slice(&size.width.to_le_bytes());
            out.extend_from_slice(&size.height.to_le_bytes());
        }
        TargetStatus::NativeMoveEnded { window } => {
            out.push(2);
            out.extend_from_slice(&window.0.to_le_bytes());
            out.extend_from_slice(&[0; 25]);
        }
        _ => unreachable!("only move statuses use their separate kind"),
    }
}

fn decode_move_status(reader: &mut Reader<'_>) -> Result<TargetStatus, WireError> {
    let code = reader.byte()?;
    let window = WindowId(u64::from_le_bytes(reader.take()?));
    let present = decode_down(reader.byte()?)?;
    let proxy = u64::from_le_bytes(reader.take()?);
    let grab = (
        i32::from_le_bytes(reader.take()?),
        i32::from_le_bytes(reader.take()?),
    );
    let size = PixelSize::new(
        u32::from_le_bytes(reader.take()?),
        u32::from_le_bytes(reader.take()?),
    );
    match code {
        1 if present || proxy == 0 => Ok(TargetStatus::NativeMove {
            window,
            proxy: present.then_some(ProjectionId(proxy)),
            grab,
            size,
        }),
        2 if !present && proxy == 0 && grab == (0, 0) && size == PixelSize::new(0, 0) => {
            Ok(TargetStatus::NativeMoveEnded { window })
        }
        _ => Err(WireError::BadValue("native move status fields")),
    }
}

fn decode_status(code: u8, detail: u8) -> Result<TargetStatus, WireError> {
    match (code, detail) {
        (1, 0) => Ok(TargetStatus::LocalOverride),
        (2, 0) => Ok(TargetStatus::Resumed),
        (3, detail) => Ok(TargetStatus::Refused(match detail {
            1 => Refusal::Permission,
            2 => Refusal::Locked,
            3 => Refusal::SecureInput,
            4 => Refusal::Busy,
            5 => Refusal::InjectorFailed,
            _ => return Err(WireError::BadValue("unknown refusal detail")),
        })),
        _ => Err(WireError::BadValue("invalid status code or detail")),
    }
}

#[cfg(test)]
mod drag_in_wire_tests {
    use super::*;

    fn encoded(status: TargetStatus) -> (InputMessage, Vec<u8>, Frame) {
        let message = InputMessage::Status {
            session: SessionId(11),
            status,
        };
        let mut bytes = Vec::new();
        encode_input(&message, &mut bytes).unwrap();
        let mut decoder = crate::wire::FrameDecoder::new(MAX_INPUT_PAYLOAD);
        decoder.push(&bytes);
        let frame = decoder.next_frame().unwrap().unwrap();
        (message, bytes, frame)
    }

    #[test]
    fn native_move_status_round_trip_exact_42_byte_payload() {
        for proxy in [None, Some(ProjectionId(u64::MAX))] {
            let (message, bytes, frame) = encoded(TargetStatus::NativeMove {
                window: WindowId(u64::MAX),
                proxy,
                grab: (i32::MIN, i32::MAX),
                size: PixelSize::new(u32::MAX, 1),
            });
            assert_eq!(frame.kind, KIND_STATUS_MOVE);
            assert_eq!(frame.payload.len(), 42);
            assert_eq!(bytes.len(), HEADER_LEN + 42);
            assert_eq!(decode_input(&frame).unwrap(), message);
        }
        let (message, _, frame) = encoded(TargetStatus::NativeMoveEnded {
            window: WindowId(5),
        });
        assert_eq!(&frame.payload[17..], &[0; 25]);
        assert_eq!(decode_input(&frame).unwrap(), message);
    }

    #[test]
    fn native_move_status_rejects_wrong_length_and_noncanonical_ended() {
        let (_, _, frame) = encoded(TargetStatus::NativeMoveEnded {
            window: WindowId(5),
        });
        for length in 0..42 {
            let short = Frame {
                kind: frame.kind,
                payload: frame.payload[..length].to_vec(),
            };
            assert!(matches!(
                decode_input(&short),
                Err(WireError::BadLength { .. })
            ));
        }
        for offset in 17..42 {
            let mut bad = frame.clone();
            bad.payload[offset] = 1;
            assert!(decode_input(&bad).is_err());
        }
        let mut long = frame.clone();
        long.payload.push(0);
        assert!(matches!(
            decode_input(&long),
            Err(WireError::BadLength { .. })
        ));
        let mut bad_code = frame;
        bad_code.payload[8] = 3;
        assert!(decode_input(&bad_code).is_err());
    }

    #[test]
    fn native_move_keeps_old_status_kind_unchanged() {
        for status in [
            TargetStatus::LocalOverride,
            TargetStatus::Resumed,
            TargetStatus::Refused(Refusal::Locked),
        ] {
            let (message, bytes, frame) = encoded(status);
            assert_eq!(frame.kind, KIND_STATUS);
            assert_eq!(frame.payload.len(), 10);
            assert_eq!(bytes.len(), HEADER_LEN + 10);
            assert_eq!(decode_input(&frame).unwrap(), message);
        }
    }
}
