#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use crosspane_media::{
    tiles::{TileDecoder, TileEncoder},
    wire::{FrameHeader, MAGIC, MAX_FRAME_BYTES, MediaError, read_header},
};
use crosspane_types::geom::{PixelRect, PixelSize, euclid::point2};
use proptest::prelude::*;

fn header(width: u32, height: u32, seq: u64) -> FrameHeader {
    FrameHeader {
        projection: 0x0807_0605_0403_0201,
        seq,
        key: false,
        captured_ns: 0x1817_1615_1413_1211,
        width,
        height,
    }
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            // SplitMix64: deterministic, with no dependency beyond the spec's list.
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut value = state;
            value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            (value ^ (value >> 31)) as u8
        })
        .collect()
}

fn put_u16(data: &mut [u8], offset: usize, value: u16) {
    data[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(data: &mut [u8], offset: usize, value: u32) {
    data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn get_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn wire_header(width: u32, height: u32, key: bool, count: u32) -> Vec<u8> {
    let h = header(width, height, 1);
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&[1, u8::from(key), 0, 0]);
    out.extend_from_slice(&h.projection.to_le_bytes());
    out.extend_from_slice(&h.seq.to_le_bytes());
    out.extend_from_slice(&h.captured_ns.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&64_u16.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out
}

fn record(out: &mut Vec<u8>, tx: u16, ty: u16, encoding: u8, payload: &[u8]) {
    out.extend_from_slice(&tx.to_le_bytes());
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(&[encoding, 0, 0, 0]);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
}

fn solid_frame(width: u32, height: u32) -> Vec<u8> {
    let mut encoder = TileEncoder::new();
    let mut data = Vec::new();
    let pixels = [3, 7, 19, 255].repeat((width * height) as usize);
    encoder
        .encode(
            header(width, height, 1),
            &pixels,
            width * 4,
            false,
            &mut data,
        )
        .unwrap();
    data
}

fn reject(data: &[u8], expected: MediaError) {
    let mut decoder = TileDecoder::new();
    decoder.apply(&solid_frame(128, 65)).unwrap();
    let (before, size) = decoder.canvas();
    let before = before.to_vec();
    assert_eq!(decoder.apply(data), Err(expected));
    assert_eq!(decoder.canvas(), (before.as_slice(), size));
}

fn round_trip(width: u32, height: u32, seed: u64) {
    let mut pixels = noise((width * height * 4) as usize, seed);
    let mut encoder = TileEncoder::new();
    let mut decoder = TileDecoder::new();
    let mut out = Vec::new();
    for seq in 1..=4 {
        if seq > 1 {
            let index = pixels.len() / seq as usize;
            pixels[index] ^= seq as u8;
        }
        let stats = encoder
            .encode(
                header(width, height, seq),
                &pixels,
                width * 4,
                false,
                &mut out,
            )
            .unwrap()
            .unwrap();
        assert_eq!(stats.key, seq == 1);
        assert_eq!(stats.bytes, out.len());
        let (h, rects) = decoder.apply(&out).unwrap();
        assert_eq!(h.seq, seq);
        assert_eq!(rects.len(), stats.tiles as usize);
        assert_eq!(
            decoder.canvas(),
            (pixels.as_slice(), PixelSize::new(width, height))
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn random_frames_round_trip(
        (width, height) in prop_oneof![Just((1_u32, 1_u32)), Just((16384, 1)), (1_u32..258, 1_u32..194)],
        seed in any::<u64>(),
        changes in prop::collection::vec((any::<u32>(), any::<[u8; 4]>()), 2..12),
    ) {
        let mut pixels = noise((width * height * 4) as usize, seed);
        let mut encoder = TileEncoder::new();
        let mut decoder = TileDecoder::new();
        let mut out = Vec::new();
        let stats = encoder.encode(header(width, height, 1), &pixels, width * 4, false, &mut out)?.unwrap();
        prop_assert!(stats.key);
        decoder.apply(&out)?;
        prop_assert_eq!(decoder.canvas(), (pixels.as_slice(), PixelSize::new(width, height)));
        for (index, (position, pixel)) in changes.into_iter().enumerate() {
            let offset = (position as usize % (width * height) as usize) * 4;
            pixels[offset..offset + 4].copy_from_slice(&pixel);
            if let Some(stats) = encoder.encode(header(width, height, index as u64 + 2), &pixels, width * 4, false, &mut out)? {
                prop_assert!(!stats.key);
                decoder.apply(&out)?;
            }
            prop_assert_eq!(decoder.canvas(), (pixels.as_slice(), PixelSize::new(width, height)));
        }
    }
}

#[test]
fn fixed_round_trips() {
    for (width, height) in [
        (1, 1),
        (16384, 1),
        (1, 16384),
        (64, 64),
        (63, 65),
        (65, 129),
    ] {
        round_trip(width, height, 17);
    }
}

#[test]
fn stride_padding_is_ignored() {
    let (width, height, stride) = (65, 67, 65 * 4 + 17);
    let pixels = noise((width * height * 4) as usize, 9);
    let mut padded = vec![0xcc; (stride * height) as usize];
    for (source, destination) in pixels
        .chunks_exact((width * 4) as usize)
        .zip(padded.chunks_exact_mut(stride as usize))
    {
        destination[..source.len()].copy_from_slice(source);
    }
    let mut tight_encoder = TileEncoder::new();
    let mut padded_encoder = TileEncoder::new();
    let mut tight = Vec::new();
    let mut out = Vec::new();
    tight_encoder
        .encode(
            header(width, height, 1),
            &pixels,
            width * 4,
            false,
            &mut tight,
        )
        .unwrap();
    padded_encoder
        .encode(header(width, height, 1), &padded, stride, false, &mut out)
        .unwrap();
    assert_eq!(out, tight);
    let mut decoder = TileDecoder::new();
    decoder.apply(&out).unwrap();
    assert_eq!(decoder.canvas().0, pixels);
    for row in padded.chunks_exact_mut(stride as usize) {
        row[(width * 4) as usize..].fill(0xee);
    }
    // The unused padding after the final row can be absent altogether.
    padded.truncate(((height - 1) * stride + width * 4) as usize);
    assert_eq!(
        padded_encoder.encode(header(width, height, 2), &padded, stride, false, &mut out),
        Ok(None)
    );
    assert!(out.is_empty());
    padded[((height - 1) * stride + (width - 1) * 4) as usize] ^= 1;
    let stats = padded_encoder
        .encode(header(width, height, 3), &padded, stride, false, &mut out)
        .unwrap()
        .unwrap();
    assert_eq!(stats.tiles, 1);
    decoder.apply(&out).unwrap();
    let mut expected = pixels;
    expected[((height * width - 1) * 4) as usize] ^= 1;
    assert_eq!(decoder.canvas().0, expected);
}

#[test]
fn no_change_and_one_changed_pixel() {
    let (width, height) = (130, 129);
    let mut pixels = [5, 6, 7, 8].repeat((width * height) as usize);
    let mut encoder = TileEncoder::new();
    let mut decoder = TileDecoder::new();
    let mut out = Vec::new();
    encoder
        .encode(
            header(width, height, 1),
            &pixels,
            width * 4,
            false,
            &mut out,
        )
        .unwrap();
    decoder.apply(&out).unwrap();
    let mut next = header(width, height, 2);
    next.key = true; // The encoder, rather than the supplied header, decides this bit.
    assert_eq!(
        encoder.encode(next, &pixels, width * 4, false, &mut out),
        Ok(None)
    );
    assert!(out.is_empty());
    pixels[((width * 128 + 129) * 4) as usize] ^= 1;
    let stats = encoder
        .encode(
            header(width, height, 3),
            &pixels,
            width * 4,
            false,
            &mut out,
        )
        .unwrap()
        .unwrap();
    assert_eq!((stats.key, stats.tiles), (false, 1));
    let (_, rects) = decoder.apply(&out).unwrap();
    assert_eq!(rects, [PixelRect::new(point2(128, 128), point2(130, 129))]);
    assert_eq!(decoder.canvas().0, pixels);
}

#[test]
fn tile_encoding_choices() {
    let solid = solid_frame(64, 64);
    assert_eq!(solid[52], 2);
    assert_eq!(get_u32(&solid, 56), 4);
    assert_eq!(&solid[60..], &[3, 7, 19, 255]);
    for (pixels, encoding) in [
        (noise(64 * 64 * 4, 13), 0),
        (
            (0..64 * 64)
                .flat_map(|i| [(i % 2) as u8, 1, 2, 255])
                .collect(),
            1,
        ),
    ] {
        let mut encoder = TileEncoder::new();
        let mut out = Vec::new();
        encoder
            .encode(header(64, 64, 1), &pixels, 256, false, &mut out)
            .unwrap();
        assert_eq!(out[52], encoding);
        if encoding == 0 {
            assert_eq!(&out[60..], pixels);
        } else {
            assert_eq!(
                lz4_flex::decompress_size_prepended(&out[60..]).unwrap(),
                pixels
            );
        }
        let mut decoder = TileDecoder::new();
        decoder.apply(&out).unwrap();
        assert_eq!(decoder.canvas().0, pixels);
    }
}

#[test]
fn header_layout_and_header_only_parse() {
    let data = solid_frame(1, 1);
    let mut expected = header(1, 1, 1);
    expected.key = true;
    assert_eq!(&data[..8], &[0x43, 0x50, 0x46, 0x31, 1, 1, 0, 0]);
    assert_eq!(&data[8..16], &[1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(&data[16..24], &1_u64.to_le_bytes());
    assert_eq!(
        &data[24..32],
        &[0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18]
    );
    assert_eq!(
        &data[32..48],
        &[1, 0, 0, 0, 1, 0, 0, 0, 64, 0, 0, 0, 1, 0, 0, 0]
    );
    assert_eq!(read_header(&data[..48]), Ok(expected));
    let mut corrupt_tile = data;
    corrupt_tile[52] = 99;
    assert_eq!(read_header(&corrupt_tile), Ok(expected));
}

#[test]
fn resize_requests_and_force_key() {
    let mut encoder = TileEncoder::new();
    let mut decoder = TileDecoder::new();
    let mut out = Vec::new();
    for (seq, width, height, force) in [(1, 65, 64, false), (2, 64, 65, false), (3, 64, 65, true)] {
        let pixels = [1, 2, 3, 4].repeat((width * height) as usize);
        let stats = encoder
            .encode(
                header(width, height, seq),
                &pixels,
                width * 4,
                force,
                &mut out,
            )
            .unwrap()
            .unwrap();
        assert!(stats.key);
        assert_eq!(stats.tiles, width.div_ceil(64) * height.div_ceil(64));
        decoder.apply(&out).unwrap();
        assert_eq!(
            decoder.canvas(),
            (pixels.as_slice(), PixelSize::new(width, height))
        );
    }
    encoder.request_key();
    let pixels = [1, 2, 3, 4].repeat(64 * 65);
    assert!(
        encoder
            .encode(header(64, 65, 4), &pixels, 256, false, &mut out)
            .unwrap()
            .unwrap()
            .key
    );
    assert_eq!(
        encoder.encode(header(64, 65, 5), &pixels, 256, false, &mut out),
        Ok(None)
    );
}

#[test]
fn periodic_keys_include_unchanged_captures() {
    for changing in [false, true] {
        let mut encoder = TileEncoder::new();
        let mut out = Vec::new();
        let mut pixels = [1, 2, 3, 4];
        encoder
            .encode(header(1, 1, 1), &pixels, 4, false, &mut out)
            .unwrap();
        for seq in 2..=300 {
            if changing {
                pixels[0] ^= 1;
            }
            let stats = encoder
                .encode(header(1, 1, seq), &pixels, 4, false, &mut out)
                .unwrap();
            assert_eq!(stats.is_some(), changing);
            if let Some(stats) = stats {
                assert!(!stats.key);
            }
        }
        assert!(
            encoder
                .encode(header(1, 1, 301), &pixels, 4, false, &mut out)
                .unwrap()
                .unwrap()
                .key
        );
        assert_eq!(
            encoder.encode(header(1, 1, 302), &pixels, 4, false, &mut out),
            Ok(None)
        );
    }
}

#[test]
fn encoder_rejects_invalid_sizes_and_buffers_without_advancing() {
    let mut encoder = TileEncoder::new();
    let mut out = Vec::new();
    let pixels = [1, 2, 3, 4];
    encoder
        .encode(header(1, 1, 1), &pixels, 4, false, &mut out)
        .unwrap();
    for (width, height) in [(0, 1), (1, 0), (16385, 1), (1, 16385)] {
        assert_eq!(
            encoder.encode(header(width, height, 2), &pixels, 4, false, &mut out),
            Err(MediaError::BadSize)
        );
    }
    encoder.request_key();
    assert_eq!(
        encoder.encode(header(1, 1, 2), &pixels, 3, false, &mut out),
        Err(MediaError::BadPayload)
    );
    assert_eq!(
        encoder.encode(header(1, 2, 2), &pixels, 4, false, &mut out),
        Err(MediaError::BadPayload)
    );
    assert!(
        encoder
            .encode(header(1, 1, 2), &pixels, 4, false, &mut out)
            .unwrap()
            .unwrap()
            .key
    );
}

#[test]
fn encoder_rejects_frames_over_byte_limit_without_advancing() {
    let mut encoder = TileEncoder::new();
    let mut out = Vec::new();
    let small = [1, 2, 3, 4];
    encoder
        .encode(header(1, 1, 1), &small, 4, false, &mut out)
        .unwrap();
    let pixels = noise(MAX_FRAME_BYTES, 71);
    assert_eq!(
        encoder.encode(header(4096, 4096, 2), &pixels, 4096 * 4, false, &mut out),
        Err(MediaError::TooLarge)
    );
    assert!(out.is_empty());
    assert_eq!(
        encoder.encode(header(1, 1, 2), &small, 4, false, &mut out),
        Ok(None)
    );
}

#[test]
fn rejects_bad_magic() {
    let mut data = solid_frame(1, 1);
    data[0] ^= 1;
    assert_eq!(read_header(&data), Err(MediaError::BadMagic));
    reject(&data, MediaError::BadMagic);
}

#[test]
fn rejects_bad_version() {
    let mut data = solid_frame(1, 1);
    data[4] = 2;
    assert_eq!(read_header(&data), Err(MediaError::BadVersion));
    reject(&data, MediaError::BadVersion);
}

#[test]
fn rejects_bad_codec() {
    let mut data = solid_frame(1, 1);
    data[6] = 1;
    assert_eq!(read_header(&data), Err(MediaError::BadCodec));
    reject(&data, MediaError::BadCodec);
}

#[test]
fn rejects_reserved_fields() {
    for offset in [5, 7, 42, 43, 53, 54, 55] {
        let mut data = solid_frame(1, 1);
        data[offset] |= 0x80;
        if offset < 48 {
            assert_eq!(read_header(&data), Err(MediaError::BadReserved));
        }
        reject(&data, MediaError::BadReserved);
    }
}

#[test]
fn rejects_out_of_range_sizes() {
    for offset in [32, 36] {
        for value in [0, 16385, u32::MAX] {
            let mut data = solid_frame(1, 1);
            put_u32(&mut data, offset, value);
            assert_eq!(read_header(&data), Err(MediaError::BadSize));
            reject(&data, MediaError::BadSize);
        }
    }
}

#[test]
fn rejects_wrong_tile_size() {
    for size in [0, 32, 65, u16::MAX] {
        let mut data = solid_frame(1, 1);
        put_u16(&mut data, 40, size);
        reject(&data, MediaError::BadTile);
    }
}

#[test]
fn rejects_out_of_range_tile_indices() {
    for offset in [48, 50] {
        for value in [1, u16::MAX] {
            let mut data = solid_frame(1, 1);
            put_u16(&mut data, offset, value);
            reject(&data, MediaError::BadTile);
        }
    }
}

#[test]
fn rejects_duplicate_tiles() {
    let mut data = solid_frame(128, 64);
    put_u16(&mut data, 64, 0);
    reject(&data, MediaError::Duplicate);
}

#[test]
fn rejects_excessive_tile_count() {
    let mut data = solid_frame(1, 1);
    put_u32(&mut data, 44, 2);
    reject(&data, MediaError::BadTile);
}

#[test]
fn rejects_wrong_raw_length() {
    for len in [0, 3, 5] {
        let mut data = wire_header(1, 1, true, 1);
        record(&mut data, 0, 0, 0, &vec![0; len]);
        reject(&data, MediaError::BadPayload);
    }
}

#[test]
fn rejects_wrong_solid_length() {
    for len in [0, 3, 5] {
        let mut data = wire_header(1, 1, true, 1);
        record(&mut data, 0, 0, 2, &vec![0; len]);
        reject(&data, MediaError::BadPayload);
    }
}

#[test]
fn rejects_unknown_encoding() {
    let mut data = solid_frame(1, 1);
    data[52] = 3;
    reject(&data, MediaError::BadPayload);
}

#[test]
fn rejects_lz4_errors_and_wrong_sizes() {
    let mut invalid_payloads = vec![vec![], vec![4, 0, 0], vec![4, 0, 0, 0, 0xff]];
    invalid_payloads.push(lz4_flex::compress_prepend_size(&[0; 3]));
    invalid_payloads.push(lz4_flex::compress_prepend_size(&[0; 5]));
    let mut short_output = lz4_flex::compress_prepend_size(&[0; 3]);
    put_u32(&mut short_output, 0, 4);
    invalid_payloads.push(short_output);
    let mut long_output = lz4_flex::compress_prepend_size(&[0; 5]);
    put_u32(&mut long_output, 0, 4);
    invalid_payloads.push(long_output);
    invalid_payloads.push(vec![255, 255, 255, 255, 0]);
    for payload in invalid_payloads {
        let mut data = wire_header(1, 1, true, 1);
        record(&mut data, 0, 0, 1, &payload);
        reject(&data, MediaError::BadPayload);
    }
}

#[test]
fn rejects_trailing_bytes() {
    let mut data = solid_frame(1, 1);
    data.push(0);
    reject(&data, MediaError::Trailing);
}

#[test]
fn rejects_frames_over_byte_limit() {
    let mut data = solid_frame(1, 1);
    data.resize(MAX_FRAME_BYTES + 1, 0);
    assert_eq!(read_header(&data), Err(MediaError::TooLarge));
    reject(&data, MediaError::TooLarge);
    data.truncate(MAX_FRAME_BYTES);
    reject(&data, MediaError::Trailing);
}

#[test]
fn rejects_missing_key_tiles() {
    let mut data = wire_header(128, 64, true, 1);
    record(&mut data, 0, 0, 2, &[1, 2, 3, 4]);
    reject(&data, MediaError::MissingTiles);
    reject(&wire_header(1, 1, true, 0), MediaError::MissingTiles);
}

#[test]
fn rejects_delta_without_canvas() {
    let mut data = solid_frame(1, 1);
    data[5] = 0;
    let mut decoder = TileDecoder::new();
    assert_eq!(decoder.canvas(), (&[][..], PixelSize::new(0, 0)));
    assert_eq!(decoder.apply(&data), Err(MediaError::NoCanvas));
    assert_eq!(decoder.canvas(), (&[][..], PixelSize::new(0, 0)));
}

#[test]
fn rejects_delta_size_mismatch() {
    let mut data = solid_frame(1, 1);
    data[5] = 0;
    reject(&data, MediaError::SizeMismatch);
}

#[test]
fn rejects_truncated_headers_records_and_payloads() {
    let data = solid_frame(128, 65);
    for len in 0..data.len() {
        if len < 48 {
            assert_eq!(read_header(&data[..len]), Err(MediaError::Truncated));
        }
        reject(&data[..len], MediaError::Truncated);
    }
    let mut excessive_len = solid_frame(1, 1);
    excessive_len[52] = 1;
    put_u32(&mut excessive_len, 56, u32::MAX);
    reject(&excessive_len, MediaError::Truncated);
}

#[test]
fn late_delta_error_is_atomic_and_decoder_recovers() {
    let initial = solid_frame(128, 65);
    let mut delta = wire_header(128, 65, false, 2);
    record(&mut delta, 0, 0, 2, &[99, 98, 97, 96]);
    record(&mut delta, 1, 1, 2, &[88, 87, 86, 85]);
    delta[69] = 1; // Reserved byte in the second tile, after a valid changed tile.
    reject(&delta, MediaError::BadReserved);
    let mut decoder = TileDecoder::new();
    decoder.apply(&initial).unwrap();
    assert_eq!(decoder.apply(&delta), Err(MediaError::BadReserved));
    delta[69] = 0;
    let (_, rects) = decoder.apply(&delta).unwrap();
    assert_eq!(
        rects,
        [
            PixelRect::new(point2(0, 0), point2(64, 64)),
            PixelRect::new(point2(64, 64), point2(128, 65))
        ]
    );
    assert_eq!(&decoder.canvas().0[..4], &[99, 98, 97, 96]);
    let last_pixel = (128 * 65 - 1) * 4;
    assert_eq!(
        &decoder.canvas().0[last_pixel..last_pixel + 4],
        &[88, 87, 86, 85]
    );
    let empty = wire_header(128, 65, false, 0);
    let before = decoder.canvas().0.to_vec();
    assert!(decoder.apply(&empty).unwrap().1.is_empty());
    assert_eq!(decoder.canvas().0, before);
}

#[test]
#[ignore = "release-mode codec timings; run with --release --ignored --nocapture"]
fn release_timings() {
    const ITERATIONS: u32 = 40;
    let (width, height) = (1920_u32, 1080_u32);
    let base = noise((width * height * 4) as usize, 42);
    let mut changed = base.clone();
    let total_tiles = width.div_ceil(64) * height.div_ceil(64);
    let changed_tiles = total_tiles.div_ceil(10);
    for tile in 0..changed_tiles {
        let (tx, ty) = (tile % width.div_ceil(64), tile / width.div_ceil(64));
        for y in ty * 64..(ty * 64 + 64).min(height) {
            for x in tx * 64..(tx * 64 + 64).min(width) {
                changed[((y * width + x) * 4) as usize] ^= 0x55;
            }
        }
    }
    let mut encoder = TileEncoder::new();
    let mut decoder = TileDecoder::new();
    let mut key = Vec::new();
    let mut delta = Vec::new();
    encoder
        .encode(header(width, height, 1), &base, width * 4, true, &mut key)
        .unwrap();
    encoder
        .encode(
            header(width, height, 2),
            &changed,
            width * 4,
            false,
            &mut delta,
        )
        .unwrap();
    decoder.apply(&key).unwrap();
    decoder.apply(&delta).unwrap();
    let mut key_encode = Duration::ZERO;
    let mut delta_encode = Duration::ZERO;
    let mut key_decode = Duration::ZERO;
    let mut delta_decode = Duration::ZERO;
    for i in 0..ITERATIONS {
        let start = Instant::now();
        let stats = encoder
            .encode(
                header(width, height, u64::from(i) * 2 + 3),
                &base,
                width * 4,
                true,
                &mut key,
            )
            .unwrap()
            .unwrap();
        key_encode += start.elapsed();
        assert_eq!(stats.tiles, total_tiles);
        let start = Instant::now();
        let stats = encoder
            .encode(
                header(width, height, u64::from(i) * 2 + 4),
                &changed,
                width * 4,
                false,
                &mut delta,
            )
            .unwrap()
            .unwrap();
        delta_encode += start.elapsed();
        assert_eq!(stats.tiles, changed_tiles);
        let start = Instant::now();
        decoder.apply(&key).unwrap();
        key_decode += start.elapsed();
        assert_eq!(decoder.canvas().0, base);
        let start = Instant::now();
        decoder.apply(&delta).unwrap();
        delta_decode += start.elapsed();
        assert_eq!(decoder.canvas().0, changed);
    }
    println!(
        "1920x1080 deterministic noise, {ITERATIONS} iterations, {changed_tiles}/{total_tiles} tiles changed"
    );
    println!(
        "mean key encode {:.3} ms, delta encode {:.3} ms, key decode {:.3} ms, delta decode {:.3} ms",
        key_encode.as_secs_f64() * 1000.0 / f64::from(ITERATIONS),
        delta_encode.as_secs_f64() * 1000.0 / f64::from(ITERATIONS),
        key_decode.as_secs_f64() * 1000.0 / f64::from(ITERATIONS),
        delta_decode.as_secs_f64() * 1000.0 / f64::from(ITERATIONS)
    );
}
