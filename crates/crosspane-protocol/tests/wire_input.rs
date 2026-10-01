#![allow(clippy::unwrap_used, clippy::expect_used)]

use crosspane_protocol::msg::{InputMessage, MAX_HELD_KEYS, PointerMessage, Refusal, TargetStatus};
use crosspane_protocol::wire::{
    Frame, FrameDecoder, HEADER_LEN, KIND_ACK, KIND_BUTTON, KIND_KEY, KIND_LOCK_KEYS, KIND_POINTER,
    KIND_PROJ_HELD, KIND_SCROLL, KIND_STATE, KIND_STATUS, MAX_INPUT_PAYLOAD, WIRE_VERSION,
    WireError, decode_input, decode_pointer, encode_input, encode_pointer,
};
use crosspane_types::geom::{PointDevice, VectorLogical};
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::{DisplayId, SessionId};
use crosspane_types::input::{LockKeys, ScrollDelta, ScrollPhase};
use proptest::prelude::*;

const PHASES: [ScrollPhase; 9] = [
    ScrollPhase::Discrete,
    ScrollPhase::MayBegin,
    ScrollPhase::Began,
    ScrollPhase::Changed,
    ScrollPhase::Ended,
    ScrollPhase::Cancelled,
    ScrollPhase::MomentumBegan,
    ScrollPhase::MomentumChanged,
    ScrollPhase::MomentumEnded,
];

fn one_frame(bytes: &[u8]) -> Frame {
    let mut decoder = FrameDecoder::new(MAX_INPUT_PAYLOAD);
    decoder.push(bytes);
    let frame = decoder.next_frame().unwrap().unwrap();
    assert_eq!(decoder.next_frame(), Ok(None));
    frame
}

fn header(kind: u8, len: u32) -> Vec<u8> {
    let mut bytes = vec![WIRE_VERSION, kind, 0, 0];
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes
}

fn frame(kind: u8, len: usize) -> Frame {
    Frame {
        kind,
        payload: vec![0; len],
    }
}

fn pointer() -> PointerMessage {
    PointerMessage {
        session: SessionId(1),
        seq: 3,
        display: DisplayId(2),
        position: PointDevice::new(1.5, 2.0),
    }
}

fn assert_bad_value(result: Result<InputMessage, WireError>) {
    assert!(matches!(result, Err(WireError::BadValue(_))), "{result:?}");
}

fn assert_sticky_header_error(bytes: &[u8], error: WireError) {
    let mut decoder = FrameDecoder::new(MAX_INPUT_PAYLOAD);
    for byte in &bytes[..HEADER_LEN - 1] {
        decoder.push(&[*byte]);
        assert_eq!(decoder.next_frame(), Ok(None));
    }
    decoder.push(&bytes[HEADER_LEN - 1..]);
    assert_eq!(decoder.next_frame(), Err(error.clone()));
    assert_eq!(decoder.next_frame(), Err(error.clone()));
    decoder.push(&encode_pointer(&pointer()).unwrap());
    assert_eq!(decoder.next_frame(), Err(error));
}

#[test]
fn golden_ack() {
    let msg = InputMessage::Ack {
        session: SessionId(0x0102_0304_0506_0708),
        seq: 0x0a0b_0c0d,
    };
    let expected = [
        0x01, 0x06, 0, 0, 0x0c, 0, 0, 0, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, 0x0d,
        0x0c, 0x0b, 0x0a,
    ];
    let mut out = Vec::new();
    encode_input(&msg, &mut out).unwrap();
    assert_eq!(out, expected);
    assert_eq!(decode_input(&one_frame(&expected)), Ok(msg));
}

#[test]
fn golden_key() {
    let msg = InputMessage::Key {
        session: SessionId(1),
        seq: 2,
        usage: HidUsage { page: 7, id: 4 },
        down: true,
    };
    let expected = [
        0x01, 0x01, 0, 0, 0x11, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 7, 0, 4, 0, 1,
    ];
    let mut out = Vec::new();
    encode_input(&msg, &mut out).unwrap();
    assert_eq!(out, expected);
    assert_eq!(decode_input(&one_frame(&expected)), Ok(msg));
}

#[test]
fn golden_pointer() {
    let expected = [
        0x01, 0x20, 0, 0, 0x18, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 2, 0, 0, 0, 0, 0,
        0xc0, 0x3f, 0, 0, 0, 0x40,
    ];
    assert_eq!(encode_pointer(&pointer()).unwrap(), expected);
    assert_eq!(decode_pointer(&expected), Ok(pointer()));
}

fn usage_strategy() -> impl Strategy<Value = HidUsage> {
    (any::<u16>(), any::<u16>()).prop_map(|(page, id)| HidUsage { page, id })
}

fn coordinate_strategy() -> impl Strategy<Value = f64> {
    prop_oneof![
        any::<f32>()
            .prop_filter("finite f32", |x| x.is_finite())
            .prop_map(f64::from),
        -1.0e9f64..1.0e9f64,
    ]
}

fn key_strategy() -> impl Strategy<Value = InputMessage> {
    (any::<u64>(), any::<u32>(), usage_strategy(), any::<bool>()).prop_map(
        |(session, seq, usage, down)| InputMessage::Key {
            session: SessionId(session),
            seq,
            usage,
            down,
        },
    )
}

fn button_strategy() -> impl Strategy<Value = InputMessage> {
    (any::<u64>(), any::<u32>(), 1u8..=255, any::<bool>()).prop_map(
        |(session, seq, button, down)| InputMessage::Button {
            session: SessionId(session),
            seq,
            button: MouseButton(button),
            down,
        },
    )
}

fn scroll_strategy() -> impl Strategy<Value = InputMessage> {
    (
        any::<u64>(),
        any::<u32>(),
        any::<i32>(),
        any::<i32>(),
        proptest::option::of((coordinate_strategy(), coordinate_strategy())),
        0usize..PHASES.len(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            |(session, seq, v120_x, v120_y, pixels, phase, stop_x, stop_y)| InputMessage::Scroll {
                session: SessionId(session),
                seq,
                delta: ScrollDelta {
                    v120_x,
                    v120_y,
                    pixels: pixels.map(|(x, y)| VectorLogical::new(x, y)),
                    phase: PHASES[phase],
                    stop_x,
                    stop_y,
                },
            },
        )
}

fn lock_strategy() -> impl Strategy<Value = InputMessage> {
    (
        any::<u64>(),
        any::<u32>(),
        proptest::option::of(any::<bool>()),
        proptest::option::of(any::<bool>()),
        proptest::option::of(any::<bool>()),
    )
        .prop_map(
            |(session, seq, caps_lock, num_lock, scroll_lock)| InputMessage::LockKeys {
                session: SessionId(session),
                seq,
                keys: LockKeys {
                    caps_lock,
                    num_lock,
                    scroll_lock,
                },
            },
        )
}

fn state_strategy() -> impl Strategy<Value = InputMessage> {
    (
        any::<u64>(),
        any::<u32>(),
        proptest::collection::vec(usage_strategy(), 0..=MAX_HELD_KEYS),
        any::<u16>(),
    )
        .prop_map(|(session, seq, held_keys, buttons)| InputMessage::State {
            session: SessionId(session),
            seq,
            held_keys,
            // The wire bitmap represents a set, in ascending button order on decode.
            held_buttons: (1..=16)
                .filter(|n| buttons & (1 << (n - 1)) != 0)
                .map(MouseButton)
                .collect(),
        })
}

fn ack_strategy() -> impl Strategy<Value = InputMessage> {
    (any::<u64>(), any::<u32>()).prop_map(|(session, seq)| InputMessage::Ack {
        session: SessionId(session),
        seq,
    })
}

fn status_strategy() -> impl Strategy<Value = InputMessage> {
    let statuses = vec![
        TargetStatus::LocalOverride,
        TargetStatus::Resumed,
        TargetStatus::Refused(Refusal::Permission),
        TargetStatus::Refused(Refusal::Locked),
        TargetStatus::Refused(Refusal::SecureInput),
        TargetStatus::Refused(Refusal::Busy),
        TargetStatus::Refused(Refusal::InjectorFailed),
    ];
    (any::<u64>(), proptest::sample::select(statuses)).prop_map(|(session, status)| {
        InputMessage::Status {
            session: SessionId(session),
            status,
        }
    })
}

fn input_strategy() -> impl Strategy<Value = InputMessage> {
    prop_oneof![
        key_strategy(),
        button_strategy(),
        scroll_strategy(),
        lock_strategy(),
        state_strategy(),
        ack_strategy(),
        status_strategy()
    ]
}

fn quantized(mut msg: InputMessage) -> InputMessage {
    if let InputMessage::Scroll { delta, .. } = &mut msg {
        delta.pixels = delta
            .pixels
            .map(|v| VectorLogical::new(f64::from(v.x as f32), f64::from(v.y as f32)));
    }
    msg
}

fn round_trip(msg: InputMessage) {
    let prefix = [0xab, 0xcd];
    let mut out = prefix.to_vec();
    encode_input(&msg, &mut out).unwrap();
    assert_eq!(&out[..prefix.len()], &prefix);
    assert_eq!(
        decode_input(&one_frame(&out[prefix.len()..])).unwrap(),
        quantized(msg)
    );
}

proptest! {
    #[test]
    fn round_trip_key(msg in key_strategy()) { round_trip(msg); }
    #[test]
    fn round_trip_button(msg in button_strategy()) { round_trip(msg); }
    #[test]
    fn round_trip_scroll(msg in scroll_strategy()) { round_trip(msg); }
    #[test]
    fn round_trip_lock_keys(msg in lock_strategy()) { round_trip(msg); }
    #[test]
    fn round_trip_state(msg in state_strategy()) { round_trip(msg); }
    #[test]
    fn round_trip_ack(msg in ack_strategy()) { round_trip(msg); }
    #[test]
    fn round_trip_status(msg in status_strategy()) { round_trip(msg); }

    #[test]
    fn round_trip_pointer(
        session in any::<u64>(), seq in any::<u32>(), display in any::<u32>(),
        x in coordinate_strategy(), y in coordinate_strategy(),
    ) {
        let msg = PointerMessage { session: SessionId(session), seq, display: DisplayId(display), position: PointDevice::new(x, y) };
        let bytes = encode_pointer(&msg).unwrap();
        let decoded = decode_pointer(&bytes).unwrap();
        prop_assert_eq!(decoded, PointerMessage { position: PointDevice::new(f64::from(x as f32), f64::from(y as f32)), ..msg });
        prop_assert_eq!(encode_pointer(&decoded).unwrap(), bytes);
    }

    #[test]
    fn arbitrary_chunking(
        messages in proptest::collection::vec(input_strategy(), 1..20),
        chunks in proptest::collection::vec(1usize..=128, 1..40),
    ) {
        let mut bytes = Vec::new();
        for msg in &messages { encode_input(msg, &mut bytes).unwrap(); }
        let expected: Vec<_> = messages.into_iter().map(quantized).collect();
        for chunk_sizes in [&chunks[..], &[1][..]] {
            let mut decoder = FrameDecoder::new(MAX_INPUT_PAYLOAD);
            let mut decoded = Vec::new();
            let mut offset = 0;
            for size in chunk_sizes.iter().cycle() {
                if offset == bytes.len() { break; }
                let end = (offset + size).min(bytes.len());
                decoder.push(&bytes[offset..end]);
                offset = end;
                while let Some(frame) = decoder.next_frame().unwrap() {
                    decoded.push(decode_input(&frame).unwrap());
                }
            }
            prop_assert_eq!(&decoded, &expected);
            prop_assert_eq!(decoder.next_frame(), Ok(None));
        }
    }
}

#[test]
fn rejects_bad_version() {
    let mut bytes = header(KIND_ACK, 12);
    bytes[0] = 2;
    assert_sticky_header_error(&bytes, WireError::BadVersion(2));
    assert_eq!(decode_pointer(&bytes), Err(WireError::BadVersion(2)));
}

#[test]
fn rejects_reserved_bytes() {
    for offset in [2, 3] {
        let mut bytes = header(KIND_ACK, 12);
        bytes[offset] = 1;
        assert_sticky_header_error(&bytes, WireError::BadReserved);
        assert_eq!(decode_pointer(&bytes), Err(WireError::BadReserved));
    }
}

#[test]
fn rejects_unknown_kind() {
    for kind in 0u8..=255 {
        if !(KIND_KEY..=KIND_PROJ_HELD).contains(&kind) {
            let bytes = header(kind, 0);
            let frame = one_frame(&bytes);
            assert_eq!(decode_input(&frame), Err(WireError::BadKind(kind)));
        }
        if kind != KIND_POINTER {
            assert_eq!(
                decode_pointer(&header(kind, 24)),
                Err(WireError::BadKind(kind))
            );
        }
    }
}

#[test]
fn rejects_wrong_payload_length() {
    for (kind, size) in [
        (KIND_KEY, 17),
        (KIND_BUTTON, 14),
        (KIND_SCROLL, 30),
        (KIND_LOCK_KEYS, 15),
        (KIND_ACK, 12),
        (KIND_STATUS, 10),
    ] {
        for len in [0, size - 1, size + 1, MAX_INPUT_PAYLOAD + 1] {
            assert_eq!(
                decode_input(&frame(kind, len)),
                Err(WireError::BadLength { kind, len })
            );
        }
    }
    for len in 0..15 {
        assert_eq!(
            decode_input(&frame(KIND_STATE, len)),
            Err(WireError::BadLength {
                kind: KIND_STATE,
                len
            })
        );
    }
    for len in [0, 23, 25] {
        assert_eq!(
            decode_pointer(&header(KIND_POINTER, len)),
            Err(WireError::BadLength {
                kind: KIND_POINTER,
                len: len as usize
            })
        );
    }
}

#[test]
fn rejects_state_count_length_mismatch() {
    for (len, count) in [(15, 1), (19, 0), (18, 1)] {
        let mut state = frame(KIND_STATE, len);
        state.payload[14] = count;
        assert_eq!(
            decode_input(&state),
            Err(WireError::BadLength {
                kind: KIND_STATE,
                len
            })
        );
    }
}

#[test]
fn rejects_state_count_over_limit() {
    for count in [33, 255] {
        let mut state = frame(KIND_STATE, 15 + 4 * usize::from(count));
        state.payload[14] = count;
        assert_bad_value(decode_input(&state));
    }
}

#[test]
fn rejects_bad_down() {
    for (kind, len, offset) in [(KIND_KEY, 17, 16), (KIND_BUTTON, 14, 13)] {
        for code in [2, 255] {
            let mut msg = frame(kind, len);
            msg.payload[12] = 1;
            msg.payload[offset] = code;
            assert_bad_value(decode_input(&msg));
        }
    }
}

#[test]
fn rejects_button_zero() {
    assert_bad_value(decode_input(&frame(KIND_BUTTON, 14)));
    for msg in [
        InputMessage::Button {
            session: SessionId(1),
            seq: 1,
            button: MouseButton(0),
            down: false,
        },
        InputMessage::State {
            session: SessionId(1),
            seq: 1,
            held_keys: vec![],
            held_buttons: vec![MouseButton(0)],
        },
    ] {
        let mut out = vec![42];
        assert!(matches!(
            encode_input(&msg, &mut out),
            Err(WireError::BadValue(_))
        ));
        assert_eq!(out, [42]);
    }
}

#[test]
fn rejects_unknown_scroll_phase() {
    for code in [9, 255] {
        let mut scroll = frame(KIND_SCROLL, 30);
        scroll.payload[29] = code;
        assert_bad_value(decode_input(&scroll));
    }
}

#[test]
fn rejects_unknown_scroll_flags() {
    for flags in [8, 16, 32, 64, 128, 255] {
        let mut scroll = frame(KIND_SCROLL, 30);
        scroll.payload[28] = flags;
        assert_bad_value(decode_input(&scroll));
    }
}

#[test]
fn rejects_bad_lock_key_code() {
    for offset in 12..15 {
        for code in [3, 255] {
            let mut keys = frame(KIND_LOCK_KEYS, 15);
            keys.payload[offset] = code;
            assert_bad_value(decode_input(&keys));
        }
    }
}

#[test]
fn rejects_bad_status_code() {
    for code in [0, 4, 255] {
        let mut status = frame(KIND_STATUS, 10);
        status.payload[8] = code;
        assert_bad_value(decode_input(&status));
    }
    for code in [1, 2] {
        let mut status = frame(KIND_STATUS, 10);
        status.payload[8..].copy_from_slice(&[code, 1]);
        assert_bad_value(decode_input(&status));
    }
}

#[test]
fn rejects_bad_refusal_detail() {
    for detail in [0, 6, 255] {
        let mut status = frame(KIND_STATUS, 10);
        status.payload[8..].copy_from_slice(&[3, detail]);
        assert_bad_value(decode_input(&status));
    }
}

#[test]
fn rejects_non_finite_pointer() {
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        for offset in [24, 28] {
            let mut bytes = encode_pointer(&pointer()).unwrap();
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert!(matches!(
                decode_pointer(&bytes),
                Err(WireError::BadValue(_))
            ));
        }
    }
    for value in [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::MAX,
        -f64::MAX,
    ] {
        for position in [PointDevice::new(value, 0.0), PointDevice::new(0.0, value)] {
            assert!(matches!(
                encode_pointer(&PointerMessage {
                    position,
                    ..pointer()
                }),
                Err(WireError::BadValue(_))
            ));
        }
    }
}

#[test]
fn rejects_non_finite_scroll() {
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        for offset in [20, 24] {
            let mut scroll = frame(KIND_SCROLL, 30);
            scroll.payload[28] = 1;
            scroll.payload[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert_bad_value(decode_input(&scroll));
            scroll.payload[28] = 0;
            let decoded = decode_input(&scroll).unwrap();
            assert!(matches!(
                &decoded,
                InputMessage::Scroll {
                    delta: ScrollDelta { pixels: None, .. },
                    ..
                }
            ));
            let mut out = Vec::new();
            encode_input(&decoded, &mut out).unwrap();
            assert_eq!(&one_frame(&out).payload[20..28], &[0; 8]);
        }
    }
    for value in [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::MAX,
        -f64::MAX,
    ] {
        for pixels in [
            VectorLogical::new(value, 0.0),
            VectorLogical::new(0.0, value),
        ] {
            let msg = InputMessage::Scroll {
                session: SessionId(1),
                seq: 1,
                delta: ScrollDelta {
                    v120_x: 0,
                    v120_y: 0,
                    pixels: Some(pixels),
                    phase: ScrollPhase::Discrete,
                    stop_x: false,
                    stop_y: false,
                },
            };
            let mut out = vec![42];
            assert!(matches!(
                encode_input(&msg, &mut out),
                Err(WireError::BadValue(_))
            ));
            assert_eq!(out, [42]);
        }
    }
}

#[test]
fn rejects_pointer_trailing_bytes() {
    let mut bytes = encode_pointer(&pointer()).unwrap();
    for len in 0..bytes.len() {
        assert_eq!(decode_pointer(&bytes[..len]), Err(WireError::Truncated));
    }
    bytes.push(0);
    assert_eq!(
        decode_pointer(&bytes),
        Err(WireError::BadLength {
            kind: KIND_POINTER,
            len: 25
        })
    );
    bytes.extend_from_slice(&encode_pointer(&pointer()).unwrap());
    assert!(matches!(
        decode_pointer(&bytes),
        Err(WireError::BadLength { .. })
    ));
}

#[test]
fn rejects_encoding_state_with_33_keys() {
    let msg = InputMessage::State {
        session: SessionId(1),
        seq: 1,
        held_keys: vec![HidUsage::keyboard(4); 33],
        held_buttons: vec![],
    };
    let mut out = vec![42];
    assert!(matches!(
        encode_input(&msg, &mut out),
        Err(WireError::BadValue(_))
    ));
    assert_eq!(out, [42]);
}

#[test]
fn rejects_encoding_state_with_button_17() {
    let msg = InputMessage::State {
        session: SessionId(1),
        seq: 1,
        held_keys: vec![],
        held_buttons: vec![MouseButton(17)],
    };
    let mut out = vec![42];
    assert!(matches!(
        encode_input(&msg, &mut out),
        Err(WireError::BadValue(_))
    ));
    assert_eq!(out, [42]);
}

#[test]
fn oversized_header_is_rejected_before_payload_allocation() {
    let bytes = header(KIND_KEY, u32::MAX);
    let error = WireError::TooLarge {
        len: u32::MAX as usize,
        max: MAX_INPUT_PAYLOAD,
    };
    assert_sticky_header_error(&bytes, error.clone());
    assert_eq!(decode_pointer(&bytes), Err(error));
}
