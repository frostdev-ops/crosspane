#![allow(clippy::unwrap_used, clippy::expect_used)]

use crosspane_media::{
    tiles::{TileDecoder, TileEncoder},
    wire::{
        Codec, FrameHeader, MAX_FRAME_BYTES, MediaError, read_codec, read_header, read_video,
        write_video,
    },
};
use proptest::prelude::*;

fn header(key: bool) -> FrameHeader {
    FrameHeader {
        projection: 0x0807_0605_0403_0201,
        seq: 0x100f_0e0d_0c0b_0a09,
        key,
        captured_ns: 0x1817_1615_1413_1211,
        width: 257,
        height: 129,
    }
}

fn video(key: bool, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    write_video(header(key), payload, &mut out).unwrap();
    out
}

fn put_u32(data: &mut [u8], offset: usize, value: u32) {
    data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn video_round_trip(
        projection in any::<u64>(),
        seq in any::<u64>(),
        captured_ns in any::<u64>(),
        key in any::<bool>(),
        width in 1_u32..=16384,
        height in 1_u32..=16384,
        payload in prop::collection::vec(any::<u8>(), 0..=64 * 1024),
    ) {
        let expected = FrameHeader { projection, seq, key, captured_ns, width, height };
        let mut out = vec![99; 100];
        write_video(expected, &payload, &mut out).unwrap();
        let (actual, borrowed) = read_video(&out).unwrap();
        prop_assert_eq!(actual, expected);
        prop_assert_eq!(borrowed, payload.as_slice());
        prop_assert_eq!(borrowed.as_ptr(), out[48..].as_ptr());
        prop_assert_eq!(out.len(), 48 + payload.len());
        prop_assert_eq!(read_header(&out[..48]), Ok(expected));
        prop_assert_eq!(read_codec(&out[..7]), Ok(Codec::H264));
    }
}

#[test]
fn video_layout_is_little_endian_cpf1() {
    let payload = [0, 0, 0, 1, 0x67, 0, 0, 1, 0x68, 0, 0, 1, 0x65];
    let out = video(true, &payload);
    let expected = [
        0x43, 0x50, 0x46, 0x31, 1, 1, 1, 0, // magic, version, flags, codec, reserved
        1, 2, 3, 4, 5, 6, 7, 8, // projection
        9, 10, 11, 12, 13, 14, 15, 16, // seq
        17, 18, 19, 20, 21, 22, 23, 24, // captured_ns
        1, 1, 0, 0, 129, 0, 0, 0, // width, height
        0, 0, 0, 0, // tile-size and reserved
        13, 0, 0, 0, // access-unit byte count
    ];
    assert_eq!(&out[..48], &expected);
    assert_eq!(&out[48..], &payload);
    assert_eq!(read_video(&out), Ok((header(true), payload.as_slice())));
}

#[test]
fn video_round_trips_empty_and_64_kib_payloads_at_dimension_limits() {
    for key in [false, true] {
        for (width, height) in [(1, 1), (16384, 1), (1, 16384), (16384, 16384)] {
            for len in [0, 64 * 1024] {
                let expected = FrameHeader {
                    width,
                    height,
                    ..header(key)
                };
                let payload = vec![0xa5; len];
                let mut out = vec![0xff; 100];
                write_video(expected, &payload, &mut out).unwrap();
                assert_eq!(read_video(&out), Ok((expected, payload.as_slice())));
                assert_eq!(read_header(&out), Ok(expected));
            }
        }
    }
}

#[test]
fn codec_reader_checks_only_magic_version_and_codec() {
    for (codec_byte, codec) in [(0, Codec::Tiles), (1, Codec::H264)] {
        let mut data = video(false, &[]);
        data[5] = 0xff;
        data[6] = codec_byte;
        data[7..].fill(0xff);
        assert_eq!(read_codec(&data[..7]), Ok(codec));
        assert_eq!(read_codec(&data), Ok(codec));
    }
}

#[test]
fn video_rejects_bad_magic_version_and_unknown_codecs() {
    for (offset, value, error) in [
        (0, 0, MediaError::BadMagic),
        (4, 0, MediaError::BadVersion),
        (4, 2, MediaError::BadVersion),
        (6, 3, MediaError::BadCodec),
        (6, 255, MediaError::BadCodec),
    ] {
        let mut data = video(false, &[]);
        data[offset] = value;
        assert_eq!(read_codec(&data).as_ref().err(), Some(&error));
        assert_eq!(read_header(&data).as_ref().err(), Some(&error));
        assert_eq!(read_video(&data).as_ref().err(), Some(&error));
    }
}

#[test]
fn video_rejects_reserved_flags_and_fields() {
    for offset in [5, 7, 42, 43] {
        for value in [2, 0x80, 0xff] {
            let mut data = video(true, &[0, 0, 1, 0x65]);
            data[offset] |= value;
            assert_eq!(read_header(&data), Err(MediaError::BadReserved));
            assert_eq!(read_video(&data), Err(MediaError::BadReserved));
            assert_eq!(read_codec(&data), Ok(Codec::H264));
        }
    }
}

#[test]
fn video_rejects_bad_dimensions_and_writer_clears_output() {
    for offset in [32, 36] {
        for value in [0, 16385, u32::MAX] {
            let mut data = video(false, &[]);
            put_u32(&mut data, offset, value);
            assert_eq!(read_header(&data), Err(MediaError::BadSize));
            assert_eq!(read_video(&data), Err(MediaError::BadSize));
            let mut invalid = header(false);
            if offset == 32 {
                invalid.width = value;
            } else {
                invalid.height = value;
            }
            let mut out = vec![1, 2, 3];
            assert_eq!(
                write_video(invalid, &[], &mut out),
                Err(MediaError::BadSize)
            );
            assert!(out.is_empty());
        }
    }
}

#[test]
fn video_requires_zero_tile_size() {
    for size in [1_u16, 32, 64, 65, u16::MAX] {
        let mut data = video(false, &[]);
        data[40..42].copy_from_slice(&size.to_le_bytes());
        assert_eq!(read_header(&data), Err(MediaError::BadCodec));
        assert_eq!(read_video(&data), Err(MediaError::BadCodec));
    }
}

#[test]
fn video_rejects_truncated_and_trailing_access_units() {
    for key in [false, true] {
        let data = video(key, &[0, 0, 1, 0x65, 7]);
        for len in 0..data.len() {
            assert_eq!(read_video(&data[..len]), Err(MediaError::Truncated));
            if len < 48 {
                assert_eq!(read_header(&data[..len]), Err(MediaError::Truncated));
            }
            if len < 7 {
                assert_eq!(read_codec(&data[..len]), Err(MediaError::Truncated));
            }
        }
        for count in [0, 4, 6, u32::MAX] {
            let mut wrong_length = data.clone();
            put_u32(&mut wrong_length, 44, count);
            assert_eq!(read_header(&wrong_length[..48]), Ok(header(key)));
            let error = if count < 5 {
                MediaError::Trailing
            } else {
                MediaError::Truncated
            };
            assert_eq!(read_video(&wrong_length), Err(error));
        }
        let mut trailing = data;
        trailing.push(0);
        assert_eq!(read_video(&trailing), Err(MediaError::Trailing));
    }
}

#[test]
fn video_enforces_total_frame_byte_limit() {
    let mut payload = vec![0; MAX_FRAME_BYTES - 48];
    let mut out = Vec::new();
    write_video(header(true), &payload, &mut out).unwrap();
    assert_eq!(out.len(), MAX_FRAME_BYTES);
    assert_eq!(read_header(&out), Ok(header(true)));
    assert_eq!(read_video(&out), Ok((header(true), payload.as_slice())));
    out.push(0);
    assert_eq!(read_header(&out), Err(MediaError::TooLarge));
    assert_eq!(read_video(&out), Err(MediaError::TooLarge));
    payload.push(0);
    assert_eq!(
        write_video(header(true), &payload, &mut out),
        Err(MediaError::TooLarge)
    );
    assert!(out.is_empty());
}

#[test]
fn both_header_readers_accept_tiles_and_video_reader_rejects_tiles() {
    let expected = FrameHeader {
        width: 1,
        height: 1,
        key: true,
        ..header(false)
    };
    let mut data = Vec::new();
    TileEncoder::new()
        .encode(expected, &[1, 2, 3, 4], 4, false, &mut data)
        .unwrap();
    assert_eq!(read_codec(&data), Ok(Codec::Tiles));
    assert_eq!(read_header(&data[..48]), Ok(expected));
    assert_eq!(read_header(&data), Ok(expected));
    assert_eq!(read_video(&data), Err(MediaError::BadCodec));
    assert_eq!(read_video(&data[..48]), Err(MediaError::BadCodec));
}

#[test]
fn tile_decoder_rejects_video_without_changing_canvas() {
    let mut decoder = TileDecoder::new();
    for key in [false, true] {
        assert_eq!(decoder.apply(&video(key, &[])), Err(MediaError::BadCodec));
        assert!(decoder.canvas().0.is_empty());
    }
    let mut tiles = Vec::new();
    TileEncoder::new()
        .encode(
            header(false),
            &vec![7; 257 * 129 * 4],
            257 * 4,
            false,
            &mut tiles,
        )
        .unwrap();
    decoder.apply(&tiles).unwrap();
    let (canvas, size) = decoder.canvas();
    let before = canvas.to_vec();
    for key in [false, true] {
        for payload in [&[][..], &[0, 0, 1, 0x65][..]] {
            assert_eq!(
                decoder.apply(&video(key, payload)),
                Err(MediaError::BadCodec)
            );
            assert_eq!(decoder.canvas(), (before.as_slice(), size));
        }
    }
}
