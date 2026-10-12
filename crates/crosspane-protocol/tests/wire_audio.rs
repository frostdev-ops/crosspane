use crosspane_protocol::audio::{
    AudioKind, AudioPacket, AudioStreamId, PROJECTION_AUDIO_FEATURE, decode_audio, encode_audio,
};
use crosspane_protocol::msg::{Capability, ControlMessage, Refusal};
use crosspane_protocol::wire::{Frame, HEADER_LEN, KIND_CONTROL, decode_control, encode_control};
use proptest::prelude::*;

fn control(
    message: &ControlMessage,
) -> Result<ControlMessage, crosspane_protocol::wire::WireError> {
    let mut out = Vec::new();
    encode_control(message, &mut out)?;
    decode_control(&Frame {
        kind: KIND_CONTROL,
        payload: out[HEADER_LEN..].to_vec(),
    })
}

#[test]
fn audio_control_round_trips_and_validates_formats() {
    for stream in [AudioStreamId(1), AudioStreamId(u16::MAX)] {
        for kind in [AudioKind::Speaker, AudioKind::Microphone] {
            let message = ControlMessage::AudioOpen {
                stream,
                kind,
                channels: kind.format().channels as u8,
            };
            assert_eq!(control(&message), Ok(message));
            assert!(
                control(&ControlMessage::AudioOpen {
                    stream,
                    kind,
                    channels: 3
                })
                .is_err()
            );
        }
        for message in [
            ControlMessage::AudioOpened { stream },
            ControlMessage::AudioClose { stream },
            ControlMessage::AudioRefused {
                stream,
                reason: Refusal::Permission,
            },
            ControlMessage::ProjectionAudioOpen { stream },
        ] {
            assert_eq!(control(&message), Ok(message));
        }
    }
    assert!(
        control(&ControlMessage::AudioClose {
            stream: AudioStreamId(0)
        })
        .is_err()
    );
    assert!(
        control(&ControlMessage::ProjectionAudioOpen {
            stream: AudioStreamId(0)
        })
        .is_err()
    );
    let grants = ControlMessage::Grants(vec![Capability::AudioSpeaker, Capability::AudioMic]);
    assert_eq!(control(&grants), Ok(grants));
    // AudioOpen: unknown kind, wrong channels, zero/overflow identifier.
    for body in [
        vec![8, 1, 16, 3, 24, 2],
        vec![8, 1, 16, 1, 24, 1],
        vec![16, 1, 24, 2],
        vec![8, 128, 128, 4, 16, 1, 24, 2],
    ] {
        let mut payload = vec![0x72, body.len() as u8];
        payload.extend_from_slice(&body);
        assert!(
            decode_control(&Frame {
                kind: KIND_CONTROL,
                payload
            })
            .is_err()
        );
    }
}

#[test]
fn projection_audio_is_tag_22_and_shares_the_audio_stream_checks() {
    assert_eq!(PROJECTION_AUDIO_FEATURE, "audio/1");
    // Field 22, wire type 2 (0xb2 0x01), then the AudioStream body: field 1 as a varint.
    let mut out = Vec::new();
    encode_control(
        &ControlMessage::ProjectionAudioOpen {
            stream: AudioStreamId(7),
        },
        &mut out,
    )
    .unwrap();
    assert_eq!(&out[HEADER_LEN..], &[0xb2, 0x01, 0x02, 0x08, 0x07]);
    // Valid body, then zero and a value past u16 (the same width AudioOpen enforces).
    for (body, valid) in [
        (vec![8, 7], true),
        (vec![8, 0], false),
        (vec![8, 128, 128, 4], false),
    ] {
        let mut payload = vec![0xb2, 0x01, body.len() as u8];
        payload.extend_from_slice(&body);
        let decoded = decode_control(&Frame {
            kind: KIND_CONTROL,
            payload,
        });
        if valid {
            assert_eq!(
                decoded,
                Ok(ControlMessage::ProjectionAudioOpen {
                    stream: AudioStreamId(7)
                })
            );
        } else {
            assert!(decoded.is_err());
        }
    }
}

#[test]
fn audio_datagram_rejects_every_truncation_and_malformed_header() {
    let packet = AudioPacket {
        stream: AudioStreamId(7),
        seq: 9,
        sample_time: 480,
        opus: vec![0x55; 400],
    };
    let encoded = encode_audio(&packet).unwrap();
    assert_eq!(encoded.len(), 422);
    for end in 0..encoded.len() {
        assert!(decode_audio(&encoded[..end]).is_err());
    }
    for (index, value) in [(0, 0), (1, 0), (2, 1), (3, 1)] {
        let mut invalid = encoded.clone();
        invalid[index] = value;
        assert!(decode_audio(&invalid).is_err());
    }
    let mut zero = encoded.clone();
    zero[8..10].fill(0);
    assert!(decode_audio(&zero).is_err());
    let mut trailing = encoded;
    trailing.push(0);
    assert!(decode_audio(&trailing).is_err());
    for size in [0, 401] {
        let invalid = AudioPacket {
            opus: vec![1; size],
            ..packet.clone()
        };
        assert!(encode_audio(&invalid).is_err());
    }
    assert!(!format!("{packet:?}").contains("85, 85"));
}

proptest! {
    #[test]
    fn audio_datagram_round_trip(stream in 1u16..=u16::MAX, seq in any::<u32>(), sample_time in any::<u64>(), opus in prop::collection::vec(any::<u8>(),1..=400)) {
        let packet = AudioPacket { stream: AudioStreamId(stream), seq, sample_time, opus };
        prop_assert_eq!(decode_audio(&encode_audio(&packet).unwrap()), Ok(packet));
    }
    #[test]
    fn arbitrary_datagrams_are_bounded(bytes in prop::collection::vec(any::<u8>(),0..1024)) {
        if let Ok(packet) = decode_audio(&bytes) { prop_assert!(packet.opus.len() <= 400); }
    }
}

#[test]
fn old_peer_grants_omit_audio_capabilities_and_keep_existing_grants() {
    let all = [
        Capability::WindowShare,
        Capability::AudioSpeaker,
        Capability::AudioMic,
        Capability::InputAccept,
    ];
    assert_eq!(
        crosspane_protocol::audio::grants_for_features(&all, &[]),
        [Capability::WindowShare, Capability::InputAccept]
    );
    assert_eq!(
        crosspane_protocol::audio::grants_for_features(&all, &["video".into()]),
        [Capability::WindowShare, Capability::InputAccept]
    );
    assert_eq!(
        crosspane_protocol::audio::grants_for_features(&all, &["audio".into()]),
        all
    );
}
