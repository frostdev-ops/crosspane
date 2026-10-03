use crosspane_protocol::msg::{
    Capability, ClipFailure, ClipFetch, ClipFetchFailed, ClipFetchId, ClipOffer, ClipOfferId,
    ControlMessage, EndReason, Refusal,
};
use crosspane_protocol::wire::{Frame, KIND_CONTROL, WireError, decode_control, encode_control};
use crosspane_types::ClipKind;
use crosspane_types::color::ColorSpace;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{DisplayGeometry, PixelSize, PointDevice, PointLogical, SizeMm};
use crosspane_types::id::{DisplayId, SessionId};
use crosspane_types::input::LockKeys;

#[derive(Clone, Copy, Debug)]
enum Field {
    ColorSpace,
    Caps,
    Num,
    Scroll,
    Refusal,
    EndReason,
    Capabilities,
    OfferKinds,
    FetchKind,
    ClipFailure,
}

const FIELDS: [Field; 10] = [
    Field::ColorSpace,
    Field::Caps,
    Field::Num,
    Field::Scroll,
    Field::Refusal,
    Field::EndReason,
    Field::Capabilities,
    Field::OfferKinds,
    Field::FetchKind,
    Field::ClipFailure,
];

// These fixture builders are independent of prost and the production encoder.
fn varint(mut value: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    while value >= 128 {
        bytes.push((value as u8 & 127) | 128);
        value >>= 7;
    }
    bytes.push(value as u8);
    bytes
}

fn number(tag: u8, value: u64) -> Vec<u8> {
    [varint(u64::from(tag) << 3), varint(value)].concat()
}

fn bytes(tag: u8, value: &[u8]) -> Vec<u8> {
    [
        varint((u64::from(tag) << 3) | 2),
        varint(value.len() as u64),
        value.to_vec(),
    ]
    .concat()
}

// Valid 1 mm / 1 pixel / scale 1 geometry; other fields have their protobuf defaults.
const DISPLAY_GEOMETRY: [u8; 31] = [
    0x19, 0, 0, 0, 0, 0, 0, 0xf0, 0x3f, 0x21, 0, 0, 0, 0, 0, 0, 0xf0, 0x3f, 0x28, 1, 0x30, 1, 0x39,
    0, 0, 0, 0, 0, 0, 0xf0, 0x3f,
];

impl Field {
    fn error(self) -> WireError {
        WireError::BadValue(match self {
            Self::ColorSpace => "color space",
            Self::Caps | Self::Num | Self::Scroll => "lock key",
            Self::Refusal => "refusal",
            Self::EndReason => "end reason",
            Self::Capabilities => "capability",
            Self::OfferKinds | Self::FetchKind => "clipboard kind",
            Self::ClipFailure => "clipboard failure",
        })
    }

    fn payload(self, value: u64, packed: bool) -> Vec<u8> {
        match self {
            Self::ColorSpace => bytes(
                2,
                &bytes(1, &[DISPLAY_GEOMETRY.to_vec(), number(11, value)].concat()),
            ),
            Self::Caps | Self::Num | Self::Scroll => {
                let tag = match self {
                    Self::Caps => 1,
                    Self::Num => 2,
                    Self::Scroll => 3,
                    _ => unreachable!(),
                };
                bytes(4, &bytes(5, &number(tag, value)))
            }
            Self::Refusal => bytes(6, &number(2, value)),
            Self::EndReason => bytes(7, &number(2, value)),
            Self::Capabilities | Self::OfferKinds => {
                let (body, tag) = if matches!(self, Self::Capabilities) {
                    (8, 1)
                } else {
                    (18, 2)
                };
                bytes(
                    body,
                    &if packed {
                        bytes(tag, &varint(value))
                    } else {
                        number(tag, value)
                    },
                )
            }
            Self::FetchKind => bytes(20, &number(3, value)),
            Self::ClipFailure => bytes(21, &number(2, value)),
        }
    }

    fn assert_rejected(self, value: u64) {
        for packed in [false, true] {
            assert_eq!(
                decode_control(&Frame {
                    kind: KIND_CONTROL,
                    payload: self.payload(value, packed),
                }),
                Err(self.error()),
                "{self:?}: original wire value {value}, packed={packed}",
            );
        }
    }
}

macro_rules! range_test {
    ($name:ident, $field:ident) => {
        #[test]
        fn $name() {
            // High-bit aliases narrow to valid enum values 0 or 1 in the original codec.
            for value in [
                (1u64 << 32) + 1,
                1u64 << 32,
                (1u64 << 40) + 1,
                (1u64 << 63) + 1,
                i32::MAX as u64 + 1,
                u64::MAX,
            ] {
                Field::$field.assert_rejected(value);
            }
        }
    };
}

range_test!(
    color_space_rejects_original_out_of_range_varints,
    ColorSpace
);
range_test!(caps_rejects_original_out_of_range_varints, Caps);
range_test!(num_rejects_original_out_of_range_varints, Num);
range_test!(scroll_rejects_original_out_of_range_varints, Scroll);
range_test!(refusal_rejects_original_out_of_range_varints, Refusal);
range_test!(end_reason_rejects_original_out_of_range_varints, EndReason);
range_test!(
    capabilities_reject_original_out_of_range_varints,
    Capabilities
);
range_test!(offer_kinds_reject_original_out_of_range_varints, OfferKinds);
range_test!(fetch_kind_rejects_original_out_of_range_varints, FetchKind);
range_test!(
    clip_failure_rejects_original_out_of_range_varints,
    ClipFailure
);

#[test]
fn every_enum_rejects_negative_i32_ten_byte_varints() {
    for value in [i32::MIN, -2, -1] {
        let wire_value = value as i64 as u64;
        assert_eq!(varint(wire_value).len(), 10);
        for field in FIELDS {
            field.assert_rejected(wire_value);
        }
    }
}

#[test]
fn every_enum_rejects_unknown_in_range_values_including_i32_max() {
    for field in FIELDS {
        field.assert_rejected(99);
        field.assert_rejected(i32::MAX as u64);
    }
}

#[test]
fn packed_and_unpacked_lists_check_each_original_varint() {
    let invalid = (1u64 << 32) + 2;
    for (body, tag, error) in [(8, 1, "capability"), (18, 2, "clipboard kind")] {
        for packed in [false, true] {
            let fields = if packed {
                bytes(tag, &[vec![1], varint(invalid)].concat())
            } else {
                [number(tag, 1), number(tag, invalid)].concat()
            };
            assert_eq!(
                decode_control(&Frame {
                    kind: KIND_CONTROL,
                    payload: bytes(body, &fields)
                }),
                Err(WireError::BadValue(error)),
            );
        }
    }
}

fn assert_golden(message: ControlMessage, payload: &[u8]) {
    // Literal framing + independent payload, with no production constants or prost used.
    let mut expected = vec![1, 0x40, 0, 0];
    expected.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    expected.extend_from_slice(payload);
    let mut actual = Vec::new();
    assert_eq!(encode_control(&message, &mut actual), Ok(()));
    assert_eq!(actual, expected);
    assert_eq!(
        decode_control(&Frame {
            kind: KIND_CONTROL,
            payload: payload.to_vec()
        }),
        Ok(message),
    );
}

#[test]
fn golden_color_space_every_valid_value_including_omitted_zero() {
    for (code, color_space) in [
        (0, ColorSpace::Srgb),
        (1, ColorSpace::DisplayP3),
        (2, ColorSpace::Bt709),
    ] {
        let display = DisplayInfo {
            id: DisplayId(0),
            name: String::new(),
            geometry: DisplayGeometry {
                physical_size: SizeMm::new(1.0, 1.0),
                pixel_size: PixelSize::new(1, 1),
                scale: 1.0,
                logical_origin: PointLogical::new(0.0, 0.0),
            },
            refresh_millihz: 0,
            color_space,
            hdr: false,
        };
        let mut payload = if code == 0 {
            vec![0x12, 33, 0x0a, 31]
        } else {
            vec![0x12, 35, 0x0a, 33]
        };
        payload.extend_from_slice(&DISPLAY_GEOMETRY);
        if code != 0 {
            payload.extend_from_slice(&[0x58, code]);
        }
        assert_golden(ControlMessage::Displays(vec![display]), &payload);
    }
}

fn golden_lock(field: Field, tag: u8) {
    for (code, value) in [(0, None), (1, Some(false)), (2, Some(true))] {
        let mut lock_keys = LockKeys::default();
        match field {
            Field::Caps => lock_keys.caps_lock = value,
            Field::Num => lock_keys.num_lock = value,
            Field::Scroll => lock_keys.scroll_lock = value,
            _ => unreachable!(),
        }
        let payload = if code == 0 {
            vec![0x22, 2, 0x2a, 0]
        } else {
            vec![0x22, 4, 0x2a, 2, tag, code]
        };
        assert_golden(
            ControlMessage::StartControl {
                session: SessionId(0),
                entry_display: DisplayId(0),
                entry: PointDevice::new(0.0, 0.0),
                lock_keys,
            },
            &payload,
        );
    }
}

#[test]
fn golden_caps_every_valid_value_including_omitted_zero() {
    golden_lock(Field::Caps, 8);
}
#[test]
fn golden_num_every_valid_value_including_omitted_zero() {
    golden_lock(Field::Num, 16);
}
#[test]
fn golden_scroll_every_valid_value_including_omitted_zero() {
    golden_lock(Field::Scroll, 24);
}

#[test]
fn golden_refusal_every_valid_value() {
    for (code, reason) in [
        (1, Refusal::Permission),
        (2, Refusal::Locked),
        (3, Refusal::SecureInput),
        (4, Refusal::Busy),
        (5, Refusal::InjectorFailed),
    ] {
        assert_golden(
            ControlMessage::ControlRefused {
                session: SessionId(0),
                reason,
            },
            &[0x32, 2, 16, code],
        );
    }
}

#[test]
fn golden_end_reason_every_valid_value() {
    for (code, reason) in [
        (1, EndReason::Released),
        (2, EndReason::Panic),
        (3, EndReason::TargetLocked),
        (4, EndReason::ControllerLocked),
        (5, EndReason::LinkLost),
        (6, EndReason::Revoked),
    ] {
        assert_golden(
            ControlMessage::EndControl {
                session: SessionId(0),
                reason,
            },
            &[0x3a, 2, 16, code],
        );
    }
}

#[test]
fn golden_capabilities_every_valid_value_and_packed_list() {
    let capabilities = [
        Capability::InputAccept,
        Capability::WindowShare,
        Capability::WindowBrowse,
        Capability::WindowPresent,
        Capability::AudioSpeaker,
        Capability::AudioMic,
        Capability::ClipboardRead,
        Capability::ClipboardWrite,
    ];
    for (index, capability) in capabilities.into_iter().enumerate() {
        assert_golden(
            ControlMessage::Grants(vec![capability]),
            &[0x42, 3, 0x0a, 1, index as u8 + 1],
        );
    }
    assert_golden(
        ControlMessage::Grants(capabilities.to_vec()),
        &[0x42, 10, 0x0a, 8, 1, 2, 3, 4, 5, 6, 7, 8],
    );
    assert_golden(ControlMessage::Grants(vec![]), &[0x42, 0]);
}

#[test]
fn golden_offer_kinds_every_valid_value_and_packed_order() {
    for (code, kind) in [(1, ClipKind::Text), (2, ClipKind::Image)] {
        assert_golden(
            ControlMessage::ClipOffer(ClipOffer {
                offer: ClipOfferId(0),
                kinds: vec![kind],
            }),
            &[0x92, 1, 3, 18, 1, code],
        );
    }
    for (kinds, codes) in [
        (vec![ClipKind::Text, ClipKind::Image], [1, 2]),
        (vec![ClipKind::Image, ClipKind::Text], [2, 1]),
    ] {
        assert_golden(
            ControlMessage::ClipOffer(ClipOffer {
                offer: ClipOfferId(0),
                kinds,
            }),
            &[0x92, 1, 4, 18, 2, codes[0], codes[1]],
        );
    }
}

#[test]
fn golden_fetch_kind_every_valid_value() {
    for (code, kind) in [(1, ClipKind::Text), (2, ClipKind::Image)] {
        assert_golden(
            ControlMessage::ClipFetch(ClipFetch {
                fetch: ClipFetchId(0),
                offer: ClipOfferId(0),
                kind,
            }),
            &[0xa2, 1, 2, 24, code],
        );
    }
}

#[test]
fn golden_clip_failure_every_valid_value() {
    for (code, reason) in [
        (1, ClipFailure::Expired),
        (2, ClipFailure::Locked),
        (3, ClipFailure::NotGranted),
        (4, ClipFailure::TooLarge),
        (5, ClipFailure::Unavailable),
    ] {
        assert_golden(
            ControlMessage::ClipFetchFailed(ClipFetchFailed {
                fetch: ClipFetchId(0),
                reason,
            }),
            &[0xaa, 1, 2, 16, code],
        );
    }
}

#[test]
fn valid_unpacked_repeated_enum_fields_remain_accepted() {
    for (payload, message) in [
        (
            vec![0x42, 4, 8, 1, 8, 8],
            ControlMessage::Grants(vec![Capability::InputAccept, Capability::ClipboardWrite]),
        ),
        (
            vec![0x92, 1, 4, 16, 1, 16, 2],
            ControlMessage::ClipOffer(ClipOffer {
                offer: ClipOfferId(0),
                kinds: vec![ClipKind::Text, ClipKind::Image],
            }),
        ),
    ] {
        assert_eq!(
            decode_control(&Frame {
                kind: KIND_CONTROL,
                payload
            }),
            Ok(message)
        );
    }
}
