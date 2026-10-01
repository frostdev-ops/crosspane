#![allow(clippy::unwrap_used, clippy::expect_used)]

use crosspane_protocol::msg::{ControlMessage, InputMessage, MAX_HELD_KEYS, Refusal};
use crosspane_protocol::projection::{
    BrowsableWindow, MAX_BROWSE_WINDOWS, ParkingKind, ProjInput, ProjectionEndReason,
    ProjectionMessage, WindowSummary,
};
use crosspane_protocol::wire::{
    Frame, FrameDecoder, HEADER_LEN, KIND_CONTROL, KIND_PROJ_BUTTON, KIND_PROJ_HELD, KIND_PROJ_KEY,
    KIND_PROJ_MOTION, KIND_PROJ_SCROLL, MAX_CONTROL_PAYLOAD, MAX_INPUT_PAYLOAD, WIRE_VERSION,
    WireError, decode_control, decode_input, encode_control, encode_input,
};
use crosspane_types::geom::{PixelSize, PointDevice, VectorLogical};
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::{ProjectionId, WindowId};
use crosspane_types::input::{ScrollDelta, ScrollPhase};
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

fn finite() -> impl Strategy<Value = f64> {
    any::<f64>().prop_filter("finite", |value| value.is_finite())
}

fn position() -> impl Strategy<Value = PointDevice> {
    (finite(), finite()).prop_map(|(x, y)| PointDevice::new(x, y))
}

fn size() -> impl Strategy<Value = PixelSize> {
    (any::<u32>(), any::<u32>()).prop_map(|(w, h)| PixelSize::new(w, h))
}

fn text() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..=256).prop_map(|chars| chars.into_iter().collect())
}

fn browsable_window() -> impl Strategy<Value = BrowsableWindow> {
    (
        any::<u64>(),
        proptest::collection::vec(any::<char>(), 0..=32),
        proptest::collection::vec(any::<char>(), 0..=32),
        size(),
    )
        .prop_map(|(window, title, app_id, size)| BrowsableWindow {
            window: WindowId(window),
            summary: WindowSummary {
                title: format!("窗口 — {}", title.into_iter().collect::<String>()),
                app_id: app_id.into_iter().collect(),
            },
            size,
        })
}

fn usage() -> impl Strategy<Value = HidUsage> {
    (any::<u16>(), any::<u16>()).prop_map(|(page, id)| HidUsage { page, id })
}

fn delta() -> impl Strategy<Value = ScrollDelta> {
    (
        any::<i32>(),
        any::<i32>(),
        proptest::option::of((-1.0e9f64..1.0e9f64, -1.0e9f64..1.0e9f64)),
        0usize..PHASES.len(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            |(v120_x, v120_y, pixels, phase, stop_x, stop_y)| ScrollDelta {
                v120_x,
                v120_y,
                pixels: pixels.map(|(x, y)| VectorLogical::new(x, y)),
                phase: PHASES[phase],
                stop_x,
                stop_y,
            },
        )
}

fn one_frame(bytes: &[u8], max: usize) -> Frame {
    let mut decoder = FrameDecoder::new(max);
    // Also exercise the new kinds across every single-byte chunk boundary.
    let mut frame = None;
    for byte in bytes {
        decoder.push(&[*byte]);
        if let Some(next) = decoder.next_frame().unwrap() {
            assert!(frame.is_none());
            frame = Some(next);
        }
    }
    let frame = frame.unwrap();
    assert_eq!(decoder.next_frame(), Ok(None));
    frame
}

fn input_frame(input: &ProjInput) -> Frame {
    let mut bytes = vec![0xca, 0xfe];
    encode_input(&InputMessage::Proj(input.clone()), &mut bytes).unwrap();
    assert_eq!(&bytes[..2], &[0xca, 0xfe]);
    let bytes = &bytes[2..];
    assert_eq!(&bytes[..4], &[WIRE_VERSION, bytes[1], 0, 0]);
    assert_eq!(
        &bytes[4..HEADER_LEN],
        &((bytes.len() - HEADER_LEN) as u32).to_le_bytes()
    );
    one_frame(bytes, MAX_INPUT_PAYLOAD)
}

fn control_frame(message: &ProjectionMessage) -> Frame {
    let mut bytes = vec![0xca, 0xfe];
    encode_control(&ControlMessage::Projection(message.clone()), &mut bytes).unwrap();
    assert_eq!(&bytes[..2], &[0xca, 0xfe]);
    one_frame(&bytes[2..], MAX_CONTROL_PAYLOAD)
}

fn round_trip_input(mut input: ProjInput) {
    let frame = input_frame(&input);
    // ScrollDelta retains E1's f32 wire precision; positions retain all f64 bits.
    if let ProjInput::Scroll { delta, .. } = &mut input {
        delta.pixels = delta.pixels.map(|pixels| {
            VectorLogical::new(f64::from(pixels.x as f32), f64::from(pixels.y as f32))
        });
    }
    assert_eq!(decode_input(&frame), Ok(InputMessage::Proj(input.clone())));
    assert_eq!(input_frame(&input), frame);
}

fn round_trip_control(message: ProjectionMessage) {
    assert_eq!(
        decode_control(&control_frame(&message)),
        Ok(ControlMessage::Projection(message))
    );
}

proptest! {
    #[test]
    fn round_trip_key(p in any::<u64>(), seq in any::<u32>(), usage in usage(), down in any::<bool>()) {
        round_trip_input(ProjInput::Key { projection: ProjectionId(p), seq, usage, down });
    }

    #[test]
    fn round_trip_button(p in any::<u64>(), seq in any::<u32>(), button in 1u8..=255,
        down in any::<bool>(), position in position()) {
        round_trip_input(ProjInput::Button { projection: ProjectionId(p), seq,
            button: MouseButton(button), down, position });
    }

    #[test]
    fn round_trip_scroll(p in any::<u64>(), seq in any::<u32>(), delta in delta(), position in position()) {
        round_trip_input(ProjInput::Scroll { projection: ProjectionId(p), seq, delta, position });
    }

    #[test]
    fn round_trip_motion(p in any::<u64>(), seq in any::<u32>(), position in position()) {
        round_trip_input(ProjInput::Motion { projection: ProjectionId(p), seq, position });
    }

    #[test]
    fn round_trip_held(p in any::<u64>(), seq in any::<u32>(),
        keys in proptest::collection::vec(usage(), 0..=MAX_HELD_KEYS),
        buttons in proptest::collection::vec(1u8..=16, 0..=16)) {
        round_trip_input(ProjInput::Held { projection: ProjectionId(p), seq, keys,
            buttons: buttons.into_iter().map(MouseButton).collect() });
    }

    #[test]
    fn round_trip_start(p in any::<u64>(), title in text(), app_id in text(), size in size()) {
        round_trip_control(ProjectionMessage::Start { projection: ProjectionId(p),
            window: WindowSummary { title, app_id }, size });
    }

    #[test]
    fn round_trip_accepted(p in any::<u64>(), size in size(),
        scale in finite().prop_filter("positive", |v| *v > 0.0)) {
        round_trip_control(ProjectionMessage::Accepted { projection: ProjectionId(p), size, scale });
    }

    #[test]
    fn round_trip_refused(p in any::<u64>(), reason in proptest::sample::select(vec![
        Refusal::Permission, Refusal::Locked, Refusal::SecureInput, Refusal::Busy, Refusal::InjectorFailed])) {
        round_trip_control(ProjectionMessage::Refused { projection: ProjectionId(p), reason });
    }

    #[test]
    fn round_trip_resize(p in any::<u64>(), size in size(),
        scale in finite().prop_filter("positive", |v| *v > 0.0)) {
        round_trip_control(ProjectionMessage::Resize { projection: ProjectionId(p), size, scale });
    }

    #[test]
    fn round_trip_geometry(p in any::<u64>(), size in size(),
        parking in proptest::sample::select(vec![ParkingKind::Twin, ParkingKind::Mirror])) {
        round_trip_control(ProjectionMessage::Geometry { projection: ProjectionId(p), size, parking });
    }

    #[test]
    fn round_trip_title(p in any::<u64>(), title in text()) {
        round_trip_control(ProjectionMessage::Title { projection: ProjectionId(p), title });
    }

    #[test]
    fn round_trip_focus(p in any::<u64>(), focused in any::<bool>()) {
        round_trip_control(ProjectionMessage::Focus { projection: ProjectionId(p), focused });
    }

    #[test]
    fn round_trip_key_frame_request(p in any::<u64>()) {
        round_trip_control(ProjectionMessage::KeyFrameRequest { projection: ProjectionId(p) });
    }

    #[test]
    fn round_trip_end(p in any::<u64>(), reason in proptest::sample::select(vec![
        ProjectionEndReason::Returned, ProjectionEndReason::WindowClosed, ProjectionEndReason::Revoked,
        ProjectionEndReason::Locked, ProjectionEndReason::LinkLost, ProjectionEndReason::Failed])) {
        round_trip_control(ProjectionMessage::End { projection: ProjectionId(p), reason });
    }

    #[test]
    fn round_trip_close(p in any::<u64>(), (reason, code) in proptest::sample::select(vec![
        (ProjectionEndReason::Returned, 1u64), (ProjectionEndReason::WindowClosed, 2),
        (ProjectionEndReason::Revoked, 3), (ProjectionEndReason::Locked, 4),
        (ProjectionEndReason::LinkLost, 5), (ProjectionEndReason::Failed, 6)])) {
        let message = ProjectionMessage::Close { projection: ProjectionId(p), reason };
        let projection = if p == 0 { Vec::new() } else { number(1, p) };
        let fields = [projection, number(2, code)].concat();
        // Verify tag 10 and the reason codes independently of the codec's private pb types.
        prop_assert_eq!(control_frame(&message), projection_frame(10, &fields));
        round_trip_control(message);
    }

    #[test]
    fn round_trip_list_windows(request in any::<u32>()) {
        let message = ProjectionMessage::ListWindows { request };
        prop_assert_eq!(
            decode_control(&projection_frame(11, &number(1, u64::from(request)))),
            Ok(ControlMessage::Projection(message.clone()))
        );
        round_trip_control(message);
    }

    #[test]
    fn round_trip_window_list(request in any::<u32>(),
        windows in proptest::collection::vec(browsable_window(), MAX_BROWSE_WINDOWS)) {
        // Every case includes the empty, single-window and full lists, with Unicode titles.
        for count in [0, 1, MAX_BROWSE_WINDOWS] {
            let windows = windows[..count].to_vec();
            let mut fields = number(1, u64::from(request));
            for window in &windows {
                fields.extend(bytes_field(2, &browsable_window_bytes(window)));
            }
            let message = ProjectionMessage::WindowList { request, windows };
            prop_assert_eq!(decode_control(&projection_frame(12, &fields)),
                Ok(ControlMessage::Projection(message.clone())));
            round_trip_control(message);
        }
    }

    #[test]
    fn round_trip_pull(request in any::<u32>(), window in any::<u64>()) {
        let message = ProjectionMessage::Pull { request, window: WindowId(window) };
        let fields = [number(1, u64::from(request)), number(2, window)].concat();
        prop_assert_eq!(decode_control(&projection_frame(13, &fields)),
            Ok(ControlMessage::Projection(message.clone())));
        round_trip_control(message);
    }

    #[test]
    fn round_trip_browse_refused(request in any::<u32>(),
        (reason, code) in proptest::sample::select(vec![
            (Refusal::Permission, 1u64), (Refusal::Locked, 2), (Refusal::SecureInput, 3),
            (Refusal::Busy, 4), (Refusal::InjectorFailed, 5)])) {
        let message = ProjectionMessage::BrowseRefused { request, reason };
        let fields = [number(1, u64::from(request)), number(2, code)].concat();
        prop_assert_eq!(decode_control(&projection_frame(14, &fields)),
            Ok(ControlMessage::Projection(message.clone())));
        round_trip_control(message);
    }
}

fn samples() -> [ProjInput; 5] {
    let projection = ProjectionId(0x0102_0304_0506_0708);
    let seq = 0x0a0b_0c0d;
    let usage = HidUsage { page: 7, id: 4 };
    let position = PointDevice::new(1.5, -2.25);
    [
        ProjInput::Key {
            projection,
            seq,
            usage,
            down: true,
        },
        ProjInput::Button {
            projection,
            seq,
            button: MouseButton(255),
            down: true,
            position,
        },
        ProjInput::Scroll {
            projection,
            seq,
            delta: ScrollDelta {
                v120_x: -120,
                v120_y: 240,
                pixels: Some(VectorLogical::new(0.5, -1.0)),
                phase: ScrollPhase::MomentumChanged,
                stop_x: true,
                stop_y: false,
            },
            position,
        },
        ProjInput::Motion {
            projection,
            seq,
            position,
        },
        ProjInput::Held {
            projection,
            seq,
            keys: vec![usage],
            buttons: vec![MouseButton(16), MouseButton(1)],
        },
    ]
}

fn assert_bad_input(input: &ProjInput) {
    let mut out = vec![42];
    assert!(matches!(
        encode_input(&InputMessage::Proj(input.clone()), &mut out),
        Err(WireError::BadValue(_))
    ));
    assert_eq!(out, [42]);
}

fn assert_bad_control(message: ProjectionMessage) {
    let mut out = vec![42];
    assert!(matches!(
        encode_control(&ControlMessage::Projection(message), &mut out),
        Err(WireError::BadValue(_))
    ));
    assert_eq!(out, [42]);
}

#[test]
fn rejects_wrong_length_per_kind() {
    for (kind, size) in [
        (KIND_PROJ_KEY, 17),
        (KIND_PROJ_BUTTON, 30),
        (KIND_PROJ_SCROLL, 46),
        (KIND_PROJ_MOTION, 28),
    ] {
        for len in 0..=MAX_INPUT_PAYLOAD + 1 {
            if len == size {
                continue;
            }
            assert_eq!(
                decode_input(&Frame {
                    kind,
                    payload: vec![0; len]
                }),
                Err(WireError::BadLength { kind, len })
            );
        }
    }
    let kind = KIND_PROJ_HELD;
    for len in 0..14 {
        assert_eq!(
            decode_input(&Frame {
                kind,
                payload: vec![0; len]
            }),
            Err(WireError::BadLength { kind, len })
        );
    }
    for (keys, buttons) in [(0u8, 0u8), (1, 1), (32, 16)] {
        let expected = 14 + 4 * usize::from(keys) + usize::from(buttons);
        for len in [expected.saturating_sub(1).max(14), expected + 1] {
            if len == expected {
                continue;
            }
            let mut payload = vec![0; len];
            payload[12..14].copy_from_slice(&[keys, buttons]);
            assert_eq!(
                decode_input(&Frame { kind, payload }),
                Err(WireError::BadLength { kind, len })
            );
        }
    }
}

#[test]
fn rejects_nonzero_reserved_header_bytes() {
    let mut frames = Vec::new();
    for input in samples() {
        let mut bytes = Vec::new();
        encode_input(&InputMessage::Proj(input), &mut bytes).unwrap();
        frames.push((bytes, MAX_INPUT_PAYLOAD));
    }
    let mut bytes = Vec::new();
    encode_control(
        &ControlMessage::Projection(ProjectionMessage::KeyFrameRequest {
            projection: ProjectionId(1),
        }),
        &mut bytes,
    )
    .unwrap();
    frames.push((bytes, MAX_CONTROL_PAYLOAD));
    for (bytes, cap) in frames {
        for offset in [2, 3] {
            let mut bytes = bytes.clone();
            bytes[offset] = 1;
            let mut decoder = FrameDecoder::new(cap);
            decoder.push(&bytes);
            assert_eq!(decoder.next_frame(), Err(WireError::BadReserved));
        }
    }
}

#[test]
fn rejects_nonfinite_positions() {
    for input in &samples()[1..4] {
        let frame = input_frame(input);
        let start = match frame.kind {
            KIND_PROJ_BUTTON => 14,
            KIND_PROJ_SCROLL => 30,
            _ => 12,
        };
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for offset in [start, start + 8] {
                let mut frame = frame.clone();
                frame.payload[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
                assert!(matches!(decode_input(&frame), Err(WireError::BadValue(_))));
            }
            for position in [PointDevice::new(value, 0.0), PointDevice::new(0.0, value)] {
                let mut input = input.clone();
                match &mut input {
                    ProjInput::Button { position: p, .. }
                    | ProjInput::Scroll { position: p, .. }
                    | ProjInput::Motion { position: p, .. } => *p = position,
                    _ => unreachable!(),
                }
                assert_bad_input(&input);
            }
        }
    }
}

#[test]
fn rejects_invalid_held_counts_and_buttons() {
    let projection = ProjectionId(1);
    let keys = vec![HidUsage::keyboard(4); MAX_HELD_KEYS + 1];
    assert_bad_input(&ProjInput::Held {
        projection,
        seq: 1,
        keys,
        buttons: vec![],
    });
    assert_bad_input(&ProjInput::Held {
        projection,
        seq: 1,
        keys: vec![],
        buttons: vec![MouseButton(1); 17],
    });
    for (keys, buttons) in [(33u8, 0u8), (255, 0), (0, 17), (0, 255)] {
        let mut payload = vec![0; 14 + 4 * usize::from(keys) + usize::from(buttons)];
        payload[12..14].copy_from_slice(&[keys, buttons]);
        assert!(matches!(
            decode_input(&Frame {
                kind: KIND_PROJ_HELD,
                payload
            }),
            Err(WireError::BadValue(_))
        ));
    }
    for button in [0, 17, 255] {
        assert_bad_input(&ProjInput::Held {
            projection,
            seq: 1,
            keys: vec![],
            buttons: vec![MouseButton(button)],
        });
        let mut frame = input_frame(&samples()[4]);
        frame.payload[18] = button;
        assert!(matches!(decode_input(&frame), Err(WireError::BadValue(_))));
    }
}

#[test]
fn rejects_invalid_button_and_down_codes() {
    for (input, offset) in [(&samples()[0], 16), (&samples()[1], 13)] {
        for down in [2, 255] {
            let mut frame = input_frame(input);
            frame.payload[offset] = down;
            assert!(matches!(decode_input(&frame), Err(WireError::BadValue(_))));
        }
    }
    let mut input = samples()[1].clone();
    if let ProjInput::Button { button, .. } = &mut input {
        *button = MouseButton(0);
    }
    assert_bad_input(&input);
    let mut frame = input_frame(&samples()[1]);
    frame.payload[12] = 0;
    assert!(matches!(decode_input(&frame), Err(WireError::BadValue(_))));
}

#[test]
fn rejects_invalid_scroll_values_with_e1_absence_semantics() {
    let input = &samples()[2];
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        for offset in [20, 24] {
            let mut frame = input_frame(input);
            frame.payload[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert!(matches!(decode_input(&frame), Err(WireError::BadValue(_))));
            frame.payload[28] &= !1;
            let decoded = decode_input(&frame).unwrap();
            let mut bytes = Vec::new();
            encode_input(&decoded, &mut bytes).unwrap();
            assert_eq!(
                &one_frame(&bytes, MAX_INPUT_PAYLOAD).payload[20..28],
                &[0; 8]
            );
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
            let mut input = input.clone();
            if let ProjInput::Scroll { delta, .. } = &mut input {
                delta.pixels = Some(pixels);
            }
            assert_bad_input(&input);
        }
    }
    for (offset, codes) in [(28, vec![8, 16, 32, 64, 128, 255]), (29, vec![9, 255])] {
        for code in codes {
            let mut frame = input_frame(input);
            frame.payload[offset] = code;
            assert!(matches!(decode_input(&frame), Err(WireError::BadValue(_))));
        }
    }
}

// Independent protobuf fixtures check the documented field numbers and invalid wire values.
fn varint(mut value: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    while value >= 128 {
        bytes.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    bytes.push(value as u8);
    bytes
}

fn number(tag: u32, value: u64) -> Vec<u8> {
    [varint(u64::from(tag) << 3), varint(value)].concat()
}

fn bytes_field(tag: u32, bytes: &[u8]) -> Vec<u8> {
    [
        varint((u64::from(tag) << 3) | 2),
        varint(bytes.len() as u64),
        bytes.to_vec(),
    ]
    .concat()
}

fn double(tag: u32, value: f64) -> Vec<u8> {
    [
        varint((u64::from(tag) << 3) | 1),
        value.to_le_bytes().to_vec(),
    ]
    .concat()
}

fn projection_frame(variant: u32, fields: &[u8]) -> Frame {
    Frame {
        kind: KIND_CONTROL,
        payload: bytes_field(13, &bytes_field(variant, fields)),
    }
}

fn browsable_window_bytes(window: &BrowsableWindow) -> Vec<u8> {
    [
        number(1, window.window.0),
        bytes_field(2, window.summary.title.as_bytes()),
        bytes_field(3, window.summary.app_id.as_bytes()),
        number(4, u64::from(window.size.width)),
        number(5, u64::from(window.size.height)),
    ]
    .concat()
}

#[test]
fn rejects_too_many_browse_windows() {
    // Even empty repeated messages count toward the cap.
    assert_eq!(
        decode_control(&projection_frame(
            12,
            &bytes_field(2, &[]).repeat(MAX_BROWSE_WINDOWS + 1)
        )),
        Err(WireError::BadValue("browse window count"))
    );
    assert_bad_control(ProjectionMessage::WindowList {
        request: 0,
        windows: vec![
            BrowsableWindow {
                window: WindowId(0),
                summary: WindowSummary {
                    title: String::new(),
                    app_id: String::new(),
                },
                size: PixelSize::new(0, 0),
            };
            MAX_BROWSE_WINDOWS + 1
        ],
    });
}

#[test]
fn rejects_overlong_browse_title_and_app_id_by_utf8_byte_length() {
    for value in ["a".repeat(1025), "é".repeat(513)] {
        for field in [2, 3] {
            assert_eq!(
                decode_control(&projection_frame(
                    12,
                    &bytes_field(2, &bytes_field(field, value.as_bytes()))
                )),
                Err(WireError::BadValue("projection string"))
            );
            let mut summary = WindowSummary {
                title: String::new(),
                app_id: String::new(),
            };
            if field == 2 {
                summary.title = value.clone();
            } else {
                summary.app_id = value.clone();
            }
            assert_bad_control(ProjectionMessage::WindowList {
                request: 1,
                windows: vec![BrowsableWindow {
                    window: WindowId(1),
                    summary,
                    size: PixelSize::new(0, 0),
                }],
            });
        }
    }
    for value in ["a".repeat(1024), "é".repeat(512)] {
        // Inclusive string limits and the full u32 size range are accepted.
        round_trip_control(ProjectionMessage::WindowList {
            request: u32::MAX,
            windows: vec![
                BrowsableWindow {
                    window: WindowId(u64::MAX),
                    summary: WindowSummary {
                        title: value.clone(),
                        app_id: value.clone(),
                    },
                    size: PixelSize::new(0, u32::MAX),
                },
                BrowsableWindow {
                    window: WindowId(0),
                    summary: WindowSummary {
                        title: value.clone(),
                        app_id: value,
                    },
                    size: PixelSize::new(u32::MAX, 0),
                },
            ],
        });
    }
}

#[test]
fn rejects_unknown_browse_refusal_codes() {
    for code in [0, 6, 255, u32::MAX] {
        assert_eq!(
            decode_control(&projection_frame(14, &number(2, u64::from(code)))),
            Err(WireError::BadValue("projection refusal"))
        );
    }
    assert_eq!(
        decode_control(&projection_frame(14, &[])),
        Err(WireError::BadValue("projection refusal"))
    );
}

#[test]
fn rejects_nonfinite_or_nonpositive_scale() {
    for scale in [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        0.0,
        -0.0,
        -1.0,
        -f64::MAX,
    ] {
        for variant in [2, 4] {
            assert!(matches!(
                decode_control(&projection_frame(variant, &double(4, scale))),
                Err(WireError::BadValue(_))
            ));
        }
        for message in [
            ProjectionMessage::Accepted {
                projection: ProjectionId(1),
                size: PixelSize::new(1, 1),
                scale,
            },
            ProjectionMessage::Resize {
                projection: ProjectionId(1),
                size: PixelSize::new(1, 1),
                scale,
            },
        ] {
            assert_bad_control(message);
        }
    }
    for variant in [2, 4] {
        assert!(matches!(
            decode_control(&projection_frame(variant, &[])),
            Err(WireError::BadValue(_))
        ));
    }
}

#[test]
fn rejects_overlong_title_and_app_id_by_utf8_byte_length() {
    for value in ["a".repeat(1025), "é".repeat(513)] {
        for (variant, field) in [(1, 2), (1, 3), (6, 2)] {
            assert!(matches!(
                decode_control(&projection_frame(
                    variant,
                    &bytes_field(field, value.as_bytes())
                )),
                Err(WireError::BadValue(_))
            ));
        }
        assert_bad_control(ProjectionMessage::Title {
            projection: ProjectionId(1),
            title: value.clone(),
        });
        for window in [
            WindowSummary {
                title: value.clone(),
                app_id: String::new(),
            },
            WindowSummary {
                title: String::new(),
                app_id: value.clone(),
            },
        ] {
            assert_bad_control(ProjectionMessage::Start {
                projection: ProjectionId(1),
                window,
                size: PixelSize::new(1, 1),
            });
        }
    }
    for value in ["a".repeat(1024), "é".repeat(512)] {
        round_trip_control(ProjectionMessage::Title {
            projection: ProjectionId(1),
            title: value.clone(),
        });
        round_trip_control(ProjectionMessage::Start {
            projection: ProjectionId(1),
            window: WindowSummary {
                title: value.clone(),
                app_id: value,
            },
            size: PixelSize::new(0, u32::MAX),
        });
    }
}

#[test]
fn rejects_unknown_enum_codes() {
    for (variant, field, max) in [(3, 2, 5), (5, 4, 2), (9, 2, 6)] {
        for code in [0, max + 1, 255, u32::MAX] {
            assert!(matches!(
                decode_control(&projection_frame(variant, &number(field, u64::from(code)))),
                Err(WireError::BadValue(_))
            ));
        }
    }
}

#[test]
fn rejects_invalid_close_payloads() {
    for code in [0, 7, 255, u32::MAX] {
        assert!(matches!(
            decode_control(&projection_frame(10, &number(2, u64::from(code)))),
            Err(WireError::BadValue(_))
        ));
    }
    // An omitted reason defaults to the invalid code zero.
    assert!(matches!(
        decode_control(&projection_frame(10, &number(1, 1))),
        Err(WireError::BadValue(_))
    ));
    assert_eq!(
        decode_control(&projection_frame(10, &[0xff])),
        Err(WireError::BadControl)
    );
}

#[test]
fn unknown_projection_variant_is_unknown_control() {
    for variant in [15, 99, 536_870_911] {
        assert_eq!(
            decode_control(&projection_frame(variant, &[])),
            Err(WireError::UnknownControl)
        );
    }
    assert_eq!(
        decode_control(&Frame {
            kind: KIND_CONTROL,
            payload: bytes_field(13, &[])
        }),
        Err(WireError::UnknownControl)
    );
    assert_eq!(
        decode_control(&projection_frame(1, &[0xff])),
        Err(WireError::BadControl)
    );
}
