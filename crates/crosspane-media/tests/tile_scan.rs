#![allow(clippy::unwrap_used, clippy::expect_used)]

use crosspane_media::{
    tiles::{TileDecoder, TileEncoder},
    wire::{FrameHeader, MediaError, TILE},
};
use crosspane_types::geom::PixelSize;
use proptest::prelude::*;
use std::{
    hint::black_box,
    sync::Arc,
    time::{Duration, Instant},
};
use xxhash_rust::xxh3::{Xxh3, xxh3_64, xxh3_64_with_seed};
#[path = "support/legacy_tiles.rs"]
mod legacy;

fn header(size: PixelSize, seq: u64) -> FrameHeader {
    FrameHeader {
        projection: 1,
        seq,
        key: false,
        captured_ns: 123,
        width: size.width,
        height: size.height,
    }
}
fn emit(encoder: &mut TileEncoder, size: PixelSize, pixels: &[u8], out: &mut Vec<u8>) {
    let scan = encoder.scan(size, pixels, size.width * 4).unwrap();
    encoder
        .emit(scan, header(size, 1), pixels, size.width * 4, false, out)
        .unwrap();
}

#[test]
fn scan_is_read_only_and_counts_changes() {
    let size = PixelSize::new(129, 65);
    let mut pixels = vec![0; (size.width * size.height * 4) as usize];
    let mut encoder = TileEncoder::new();
    let first = encoder.scan(size, &pixels, size.width * 4).unwrap();
    let second = encoder.scan(size, &pixels, size.width * 4).unwrap();
    assert_eq!((first.changed(), first.total()), (6, 6));
    assert_eq!((second.changed(), second.total()), (6, 6));
    let mut out = Vec::new();
    encoder
        .emit(
            first,
            header(size, 1),
            &pixels,
            size.width * 4,
            false,
            &mut out,
        )
        .unwrap();
    pixels[0] = 1;
    assert_eq!(
        encoder
            .scan(size, &pixels, size.width * 4)
            .unwrap()
            .changed(),
        1
    );
    pixels[64 * 4] = 1;
    assert_eq!(
        encoder
            .scan(size, &pixels, size.width * 4)
            .unwrap()
            .changed(),
        2
    );
    emit(&mut encoder, size, &pixels, &mut out);
    let unchanged = encoder.scan(size, &pixels, size.width * 4).unwrap();
    assert_eq!(unchanged.changed(), 0);
    assert_eq!(
        encoder.emit(
            unchanged,
            header(size, 2),
            &pixels,
            size.width * 4,
            false,
            &mut out
        ),
        Ok(None)
    );
    assert!(out.is_empty());
    let resized = PixelSize::new(128, 65);
    let scan = encoder.scan(resized, &pixels, size.width * 4).unwrap();
    assert_eq!((scan.changed(), scan.total()), (4, 4));
}

#[test]
fn scan_ignores_padding_and_accepts_missing_final_padding() {
    let size = PixelSize::new(65, 3);
    let stride = size.width * 4 + 7;
    let mut pixels = vec![0; ((size.height - 1) * stride + size.width * 4) as usize];
    let mut encoder = TileEncoder::new();
    encoder
        .commit(encoder.scan(size, &pixels, stride).unwrap())
        .unwrap();
    for y in 0..size.height - 1 {
        pixels[(y * stride + size.width * 4) as usize..((y + 1) * stride) as usize].fill(99);
    }
    assert_eq!(encoder.scan(size, &pixels, stride).unwrap().changed(), 0);
    assert_eq!(
        encoder.scan(size, &pixels, size.width * 4 - 1).unwrap_err(),
        MediaError::BadPayload
    );
    pixels.pop();
    assert_eq!(
        encoder.scan(size, &pixels, stride).unwrap_err(),
        MediaError::BadPayload
    );
}

#[test]
fn video_commits_require_a_bit_exact_key_on_return() {
    let size = PixelSize::new(128, 65);
    let mut pixels = vec![0; (size.width * size.height * 4) as usize];
    let mut encoder = TileEncoder::new();
    let mut decoder = TileDecoder::new();
    let mut out = Vec::new();
    emit(&mut encoder, size, &pixels, &mut out);
    decoder.apply(&out).unwrap();
    for frame in 1..=305 {
        let index = frame % pixels.len();
        pixels[index] ^= 1;
        encoder
            .commit(encoder.scan(size, &pixels, size.width * 4).unwrap())
            .unwrap();
    }
    let scan = encoder.scan(size, &pixels, size.width * 4).unwrap();
    assert_eq!(scan.changed(), 0);
    let stats = encoder
        .emit(
            scan,
            header(size, 306),
            &pixels,
            size.width * 4,
            false,
            &mut out,
        )
        .unwrap()
        .unwrap();
    assert!(stats.key);
    assert_eq!(stats.tiles, 4);
    decoder.apply(&out).unwrap();
    assert_eq!(decoder.canvas().0, pixels);
}

#[test]
fn stale_and_size_mismatched_scans_change_nothing() {
    let size = PixelSize::new(1, 1);
    let pixels = [1, 2, 3, 4];
    for commit in [false, true] {
        let mut encoder = TileEncoder::new();
        let stale_emit = encoder.scan(size, &pixels, 4).unwrap();
        let stale_commit = encoder.scan(size, &pixels, 4).unwrap();
        let mut out = Vec::new();
        if commit {
            encoder
                .commit(encoder.scan(size, &pixels, 4).unwrap())
                .unwrap();
        } else {
            emit(&mut encoder, size, &pixels, &mut out);
        }
        out = vec![99];
        assert_eq!(
            encoder.emit(stale_emit, header(size, 2), &pixels, 4, true, &mut out),
            Err(MediaError::BadPayload)
        );
        assert_eq!(out, [99]);
        assert_eq!(encoder.commit(stale_commit), Err(MediaError::BadPayload));
        let valid = encoder.scan(size, &pixels, 4).unwrap();
        let mismatch = encoder.scan(size, &pixels, 4).unwrap();
        assert_eq!(
            encoder.emit(
                mismatch,
                header(PixelSize::new(2, 1), 3),
                &pixels,
                8,
                false,
                &mut out
            ),
            Err(MediaError::BadPayload)
        );
        assert_eq!(out, [99]);
        assert_eq!(valid.changed(), 0);
        let stats = encoder
            .emit(valid, header(size, 3), &pixels, 4, false, &mut out)
            .unwrap();
        assert_eq!(stats.is_some(), commit);
        if let Some(stats) = stats {
            assert!(stats.key);
        }
    }
}

#[test]
fn shared_canvas_preserves_snapshots_and_reuses_unique_storage() {
    let size = PixelSize::new(128, 65);
    let mut pixels = vec![0; (size.width * size.height * 4) as usize];
    let mut encoder = TileEncoder::new();
    let mut decoder = TileDecoder::new();
    let mut out = Vec::new();
    emit(&mut encoder, size, &pixels, &mut out);
    decoder.apply(&out).unwrap();
    let (old, old_size) = decoder.shared_canvas();
    assert_eq!(old_size, size);
    pixels[0] = 1;
    emit(&mut encoder, size, &pixels, &mut out);
    let before = Arc::as_ptr(&old);
    let mut invalid = out.clone();
    invalid.push(0);
    assert_eq!(decoder.apply(&invalid), Err(MediaError::Trailing));
    assert_eq!(Arc::as_ptr(&decoder.shared_canvas().0), before);
    decoder.apply(&out).unwrap();
    assert!(old.iter().all(|b| *b == 0));
    assert_eq!(decoder.canvas().0, pixels);
    drop(old);
    for force_key in [false, true] {
        let before = Arc::as_ptr(&decoder.shared_canvas().0);
        pixels[0] += 1;
        encoder
            .encode(
                header(size, 3),
                &pixels,
                size.width * 4,
                force_key,
                &mut out,
            )
            .unwrap();
        decoder.apply(&out).unwrap();
        assert_eq!(Arc::as_ptr(&decoder.shared_canvas().0), before);
        assert_eq!(decoder.canvas().0, pixels);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn edit_sequences_round_trip_and_match_legacy(
        width in 1_u32..=140, height in 1_u32..=80,
        edits in prop::collection::vec((any::<usize>(), any::<u8>(), any::<bool>(), any::<bool>()), 1..45),
        padding in 0_u32..8,
        random_initial in any::<bool>(),
    ) {
        let size = PixelSize::new(width, height);
        let stride = width * 4 + padding;
        let mut pixels = vec![0; ((height - 1) * stride + width * 4) as usize];
        if random_initial { pixels = noise(pixels.len()); }
        let mut split = TileEncoder::new();
        let mut encoder = TileEncoder::new();
        let mut old = legacy::LegacyEncoder::new();
        let mut decoder = TileDecoder::new();
        let (mut out, mut encoded, mut previous) = (Vec::new(), Vec::new(), Vec::new());
        for (seq, (index, value, video, key)) in edits.iter().copied().enumerate() {
            let pixel_byte = index % (width * height * 4) as usize;
            let row = pixel_byte / (width * 4) as usize;
            let col = pixel_byte % (width * 4) as usize;
            pixels[row * stride as usize + col] = value;
            let h = header(size, seq as u64);
            if seq % 7 == 0 { encoder.request_key(); old.request_key(); split.request_key(); }
            prop_assert_eq!(encoder.encode(h, &pixels, stride, key, &mut encoded).unwrap(), old.encode(h, &pixels, stride, key, &mut previous).unwrap());
            prop_assert_eq!(&encoded, &previous);
            let scan = split.scan(size, &pixels, stride).unwrap();
            if video { split.commit(scan).unwrap(); }
            else {
                if split.emit(scan, h, &pixels, stride, key, &mut out).unwrap().is_some() { decoder.apply(&out).unwrap(); }
                let expected: Vec<u8> = (0..height).flat_map(|y| pixels[(y * stride) as usize..(y * stride + width * 4) as usize].iter().copied()).collect();
                prop_assert_eq!(decoder.canvas().0, expected);
            }
        }
        let scan = split.scan(size, &pixels, stride).unwrap();
        if split.emit(scan, header(size, 100), &pixels, stride, false, &mut out).unwrap().is_some() {
            decoder.apply(&out).unwrap();
        }
        let expected: Vec<u8> = (0..height).flat_map(|y| pixels[(y * stride) as usize..(y * stride + width * 4) as usize].iter().copied()).collect();
        prop_assert_eq!(decoder.canvas().0, expected);
    }
}

fn noise(len: usize) -> Vec<u8> {
    let mut state = 42_u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}
fn hash_rows(pixels: &[u8], size: PixelSize, gather: bool, streaming: bool) -> Vec<u64> {
    let mut hashes = Vec::new();
    let mut tile = Vec::with_capacity((TILE * TILE * 4) as usize);
    for ty in 0..size.height.div_ceil(TILE) {
        for tx in 0..size.width.div_ceil(TILE) {
            tile.clear();
            let mut hasher = Xxh3::new();
            let mut seed = 0;
            let row_bytes = TILE.min(size.width - tx * TILE) as usize * 4;
            for y in ty * TILE..(ty * TILE + TILE).min(size.height) {
                let start = (y * size.width * 4 + tx * TILE * 4) as usize;
                let row = &pixels[start..start + row_bytes];
                if gather {
                    tile.extend_from_slice(row);
                } else if streaming {
                    hasher.update(row);
                } else {
                    seed = xxh3_64_with_seed(row, seed);
                }
            }
            hashes.push(if gather {
                xxh3_64(&tile)
            } else if streaming {
                hasher.digest()
            } else {
                seed
            });
        }
    }
    hashes
}
fn median(times: &mut [Duration]) -> Duration {
    times.sort();
    (times[9] + times[10]) / 2
}
#[test]
#[ignore = "release-mode 3440x1440 scan timings, median of 20"]
fn scan_timings() {
    let size = PixelSize::new(3440, 1440);
    let pixels = noise((size.width * size.height * 4) as usize);
    let encoder = TileEncoder::new();
    // Interleave and rotate the schemes so clock/thermal drift doesn't favor one scheme.
    let mut times = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    for iteration in 0..23 {
        for offset in 0..4 {
            let scheme = (iteration + offset) % 4;
            let start = Instant::now();
            match scheme {
                0 => {
                    black_box(hash_rows(black_box(&pixels), size, true, false));
                }
                1 => {
                    black_box(hash_rows(black_box(&pixels), size, false, true));
                }
                2 => {
                    black_box(hash_rows(black_box(&pixels), size, false, false));
                }
                _ => {
                    black_box(
                        encoder
                            .scan(size, black_box(&pixels), size.width * 4)
                            .unwrap(),
                    );
                }
            }
            if iteration >= 3 {
                times[scheme].push(start.elapsed());
            }
        }
    }
    let medians = times
        .each_mut()
        .map(|times| median(times).as_secs_f64() * 1000.0);
    println!(
        "3440x1440 random frame, median of 20: old gather + hash {:.3} ms, streaming {:.3} ms, row-seeded {:.3} ms, new scan {:.3} ms",
        medians[0], medians[1], medians[2], medians[3]
    );
}
