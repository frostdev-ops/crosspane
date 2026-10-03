use crosspane_protocol::clip::{
    CLIP_DATA_HEADER_LEN, ClipDataHeader, MAX_CLIP_IMAGE, MAX_CLIP_TEXT, decode_clip_data_header,
    encode_clip_data_header,
};
use crosspane_protocol::msg::{
    Capability, ClipFailure, ClipFetch, ClipFetchFailed, ClipFetchId, ClipOffer, ClipOfferId,
    ClipWithdraw, ControlMessage,
};
use crosspane_protocol::wire::{
    Frame, HEADER_LEN, KIND_CONTROL, WireError, decode_control, encode_control,
};
use crosspane_types::ClipKind;
use proptest::prelude::*;

fn encoded(message: &ControlMessage) -> Frame {
    let mut bytes = Vec::new();
    assert_eq!(encode_control(message, &mut bytes), Ok(()));
    Frame {
        kind: KIND_CONTROL,
        payload: bytes[HEADER_LEN..].to_vec(),
    }
}

fn round_trip(message: ControlMessage) {
    assert_eq!(decode_control(&encoded(&message)), Ok(message));
}

fn varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    while value >= 128 {
        out.push((value as u8 & 127) | 128);
        value >>= 7;
    }
    out.push(value as u8);
    out
}

fn number(tag: u32, value: u64) -> Vec<u8> {
    [varint(u64::from(tag) << 3), varint(value)].concat()
}

fn bytes(tag: u32, data: &[u8]) -> Vec<u8> {
    [
        varint((u64::from(tag) << 3) | 2),
        varint(data.len() as u64),
        data.to_vec(),
    ]
    .concat()
}

fn decoded(tag: u32, data: &[u8]) -> Result<ControlMessage, WireError> {
    decode_control(&Frame {
        kind: KIND_CONTROL,
        payload: bytes(tag, data),
    })
}

#[test]
fn every_clip_message_and_both_capabilities_round_trip() {
    for id in [0, 1, u64::MAX] {
        for kinds in [
            vec![ClipKind::Text],
            vec![ClipKind::Image],
            vec![ClipKind::Text, ClipKind::Image],
            vec![ClipKind::Image, ClipKind::Text],
        ] {
            round_trip(ControlMessage::ClipOffer(ClipOffer {
                offer: ClipOfferId(id),
                kinds,
            }));
        }
        round_trip(ControlMessage::ClipWithdraw(ClipWithdraw {
            offer: ClipOfferId(id),
        }));
        for kind in [ClipKind::Text, ClipKind::Image] {
            round_trip(ControlMessage::ClipFetch(ClipFetch {
                fetch: ClipFetchId(id),
                offer: ClipOfferId(id),
                kind,
            }));
        }
        for reason in [
            ClipFailure::Expired,
            ClipFailure::Locked,
            ClipFailure::NotGranted,
            ClipFailure::TooLarge,
            ClipFailure::Unavailable,
        ] {
            round_trip(ControlMessage::ClipFetchFailed(ClipFetchFailed {
                fetch: ClipFetchId(id),
                reason,
            }));
        }
    }
    round_trip(ControlMessage::Grants(vec![
        Capability::ClipboardRead,
        Capability::ClipboardWrite,
    ]));
}

#[test]
fn clipboard_control_tags_and_fields_match_independent_fixtures() {
    let offer = ControlMessage::ClipOffer(ClipOffer {
        offer: ClipOfferId(9),
        kinds: vec![ClipKind::Text, ClipKind::Image],
    });
    assert_eq!(encoded(&offer).payload, bytes(18, &[8, 9, 18, 2, 1, 2]));
    let withdraw = ControlMessage::ClipWithdraw(ClipWithdraw {
        offer: ClipOfferId(9),
    });
    assert_eq!(encoded(&withdraw).payload, bytes(19, &[8, 9]));
    for (code, kind) in [(1, ClipKind::Text), (2, ClipKind::Image)] {
        let fetch = ControlMessage::ClipFetch(ClipFetch {
            fetch: ClipFetchId(7),
            offer: ClipOfferId(9),
            kind,
        });
        assert_eq!(encoded(&fetch).payload, bytes(20, &[8, 7, 16, 9, 24, code]));
    }
    for (code, reason) in [
        (1, ClipFailure::Expired),
        (2, ClipFailure::Locked),
        (3, ClipFailure::NotGranted),
        (4, ClipFailure::TooLarge),
        (5, ClipFailure::Unavailable),
    ] {
        let failed = ControlMessage::ClipFetchFailed(ClipFetchFailed {
            fetch: ClipFetchId(7),
            reason,
        });
        assert_eq!(encoded(&failed).payload, bytes(21, &[8, 7, 16, code]));
    }
}

#[test]
fn zero_and_unknown_clipboard_enum_values_are_rejected() {
    for code in [0, 3, 255, u32::MAX as u64] {
        let offer = [number(1, 7), number(2, code)].concat();
        assert!(matches!(decoded(18, &offer), Err(WireError::BadValue(_))));
        let fetch = [number(1, 7), number(2, 9), number(3, code)].concat();
        assert!(matches!(decoded(20, &fetch), Err(WireError::BadValue(_))));
    }
    for code in [0, 6, 255, u32::MAX as u64] {
        let failed = [number(1, 7), number(2, code)].concat();
        assert!(matches!(decoded(21, &failed), Err(WireError::BadValue(_))));
    }
    for code in [0, 9, 255, u32::MAX as u64] {
        assert!(matches!(
            decoded(8, &number(1, code)),
            Err(WireError::BadValue(_))
        ));
    }
}

#[test]
fn offer_kinds_empty_duplicate_or_over_two_are_rejected_on_both_paths() {
    for kinds in [
        vec![],
        vec![ClipKind::Text; 2],
        vec![ClipKind::Image; 2],
        vec![ClipKind::Text, ClipKind::Image, ClipKind::Text],
    ] {
        let codes: Vec<_> = kinds
            .iter()
            .map(|kind| if *kind == ClipKind::Text { 1 } else { 2 })
            .collect();
        // Both protobuf representations of repeated enum fields must obey the same bound.
        let packed = [number(1, 7), bytes(2, &codes)].concat();
        let unpacked = [
            number(1, 7),
            codes
                .iter()
                .flat_map(|code| number(2, u64::from(*code)))
                .collect(),
        ]
        .concat();
        for payload in [packed, unpacked] {
            assert!(matches!(decoded(18, &payload), Err(WireError::BadValue(_))));
        }
        let mut output = vec![0xca, 0xfe];
        assert!(matches!(
            encode_control(
                &ControlMessage::ClipOffer(ClipOffer {
                    offer: ClipOfferId(7),
                    kinds
                }),
                &mut output
            ),
            Err(WireError::BadValue(_))
        ));
        assert_eq!(output, [0xca, 0xfe]);
    }
}

#[test]
fn existing_grants_keep_their_wire_numbers_and_clipboard_appends_seven_eight() {
    let old = vec![
        Capability::InputAccept,
        Capability::WindowShare,
        Capability::WindowBrowse,
        Capability::WindowPresent,
        Capability::AudioSpeaker,
        Capability::AudioMic,
    ];
    let fixture = [0x42, 8, 0x0a, 6, 1, 2, 3, 4, 5, 6];
    assert_eq!(
        encoded(&ControlMessage::Grants(old.clone())).payload,
        fixture
    );
    assert_eq!(
        decode_control(&Frame {
            kind: KIND_CONTROL,
            payload: fixture.to_vec()
        }),
        Ok(ControlMessage::Grants(old))
    );
    assert_eq!(
        encoded(&ControlMessage::Grants(vec![
            Capability::ClipboardRead,
            Capability::ClipboardWrite
        ]))
        .payload,
        [0x42, 4, 0x0a, 2, 7, 8]
    );
}

#[test]
fn clip_data_header_has_exact_thirteen_byte_little_endian_layout() {
    let header = ClipDataHeader {
        fetch: ClipFetchId(0x0807_0605_0403_0201),
        kind: ClipKind::Text,
        len: 0x1001,
    };
    let fixture = [1, 2, 3, 4, 5, 6, 7, 8, 1, 1, 0x10, 0, 0];
    assert_eq!(encode_clip_data_header(header), Ok(fixture));
    assert_eq!(decode_clip_data_header(&fixture), Ok(header));
    for end in 0..CLIP_DATA_HEADER_LEN {
        assert_eq!(
            decode_clip_data_header(&fixture[..end]),
            Err(WireError::Truncated)
        );
    }
    let mut trailing = fixture.to_vec();
    trailing.push(0);
    assert!(decode_clip_data_header(&trailing).is_err());
    for code in [0, 3, u8::MAX] {
        let mut invalid = fixture;
        invalid[8] = code;
        assert!(matches!(
            decode_clip_data_header(&invalid),
            Err(WireError::BadValue(_))
        ));
    }
}

#[test]
fn clip_data_header_caps_are_checked_before_any_content_is_present() {
    for (kind, code, cap) in [
        (ClipKind::Text, 1, MAX_CLIP_TEXT),
        (ClipKind::Image, 2, MAX_CLIP_IMAGE),
    ] {
        for len in [0, cap] {
            let header = ClipDataHeader {
                fetch: ClipFetchId(0),
                kind,
                len,
            };
            assert_eq!(
                decode_clip_data_header(&encode_clip_data_header(header).unwrap()),
                Ok(header)
            );
        }
        for len in [cap + 1, u32::MAX] {
            let header = ClipDataHeader {
                fetch: ClipFetchId(7),
                kind,
                len,
            };
            let error = Err(WireError::TooLarge {
                len: len as usize,
                max: cap as usize,
            });
            assert_eq!(encode_clip_data_header(header), error);
            let mut raw = [0; CLIP_DATA_HEADER_LEN];
            raw[..8].copy_from_slice(&7u64.to_le_bytes());
            raw[8] = code;
            raw[9..].copy_from_slice(&len.to_le_bytes());
            assert_eq!(
                decode_clip_data_header(&raw),
                Err(WireError::TooLarge {
                    len: len as usize,
                    max: cap as usize
                })
            );
        }
    }
}

proptest! {
    #[test]
    fn clip_data_headers_round_trip(fetch in any::<u64>(), image in any::<bool>(), len in 0u32..=MAX_CLIP_IMAGE) {
        let kind = if image { ClipKind::Image } else { ClipKind::Text };
        let len = if image { len } else { len % (MAX_CLIP_TEXT + 1) };
        let header = ClipDataHeader { fetch: ClipFetchId(fetch), kind, len };
        prop_assert_eq!(decode_clip_data_header(&encode_clip_data_header(header).unwrap()), Ok(header));
    }

    #[test]
    fn arbitrary_clip_data_headers_never_exceed_kind_caps(data in prop::collection::vec(any::<u8>(), 0..64)) {
        if let Ok(header) = decode_clip_data_header(&data) {
            prop_assert_eq!(data.len(), CLIP_DATA_HEADER_LEN);
            let cap = if header.kind == ClipKind::Text { MAX_CLIP_TEXT } else { MAX_CLIP_IMAGE };
            prop_assert!(header.len <= cap);
        }
    }
}
