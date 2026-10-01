//! Hot-path encodings: input-stream messages and pointer datagrams. Implemented in WP-1.1.

use crosspane_types::geom::{PointDevice, VectorLogical};
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::{DisplayId, SessionId};
use crosspane_types::input::{LockKeys, ScrollDelta, ScrollPhase};

use super::frame::decode_header;
use super::{
    Frame, HEADER_LEN, KIND_ACK, KIND_BUTTON, KIND_KEY, KIND_LOCK_KEYS, KIND_POINTER, KIND_SCROLL,
    KIND_STATE, KIND_STATUS, MAX_INPUT_PAYLOAD, WIRE_VERSION, WireError,
};
use crate::msg::{InputMessage, MAX_HELD_KEYS, PointerMessage, Refusal, TargetStatus};

/// Append one framed input message to `out`.
pub fn encode_input(msg: &InputMessage, out: &mut Vec<u8>) -> Result<(), WireError> {
    match msg {
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
    let len = frame.payload.len();
    let expected = match frame.kind {
        KIND_KEY => 17,
        KIND_BUTTON => 14,
        KIND_SCROLL => 30,
        KIND_LOCK_KEYS => 15,
        KIND_STATE if len >= 15 => len,
        KIND_STATE => 15,
        KIND_ACK => 12,
        KIND_STATUS => 10,
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
    let seq = u32::from_le_bytes(reader.take()?);
    match frame.kind {
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
