#![allow(clippy::unwrap_used, clippy::expect_used)]

use crosspane_media::{
    tiles::{TileDecoder, TileEncoder, TileSource},
    wire::{FrameHeader, MediaError, TILE, read_header},
};
use crosspane_types::geom::PixelSize;

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

struct Packed {
    width: u32,
    tiles: Vec<Option<Vec<u8>>>,
}
impl TileSource for Packed {
    fn tile(&self, tx: u32, ty: u32) -> Option<&[u8]> {
        self.tiles.get((ty * self.width + tx) as usize)?.as_deref()
    }
}
fn pack(size: PixelSize, pixels: &[u8], stride: u32) -> Packed {
    let mut tiles = Vec::new();
    for ty in 0..size.height.div_ceil(TILE) {
        for tx in 0..size.width.div_ceil(TILE) {
            let mut tile = Vec::new();
            let row_bytes = TILE.min(size.width - tx * TILE) as usize * 4;
            for y in ty * TILE..(ty * TILE + TILE).min(size.height) {
                let start = (y * stride + tx * TILE * 4) as usize;
                tile.extend_from_slice(&pixels[start..start + row_bytes]);
            }
            tiles.push(Some(tile));
        }
    }
    Packed {
        width: size.width.div_ceil(TILE),
        tiles,
    }
}
fn bits(current: &Packed, previous: Option<&Packed>) -> Vec<u32> {
    let mut bits = vec![0; current.tiles.len().div_ceil(32)];
    for (i, tile) in current.tiles.iter().enumerate() {
        if previous.is_none_or(|old| old.tiles.get(i) != Some(tile)) {
            bits[i / 32] |= 1 << (i % 32);
        }
    }
    bits
}
fn random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

#[test]
fn seeded_sequences_match_cpu_bytes() {
    let sizes = [
        (1, 1),
        (63, 65),
        (64, 64),
        (130, 70),
        (333, 211),
        (513, 257),
    ];
    let mut rng = 42;
    let mut captures = 0;
    for case in 0..500 {
        let mut cpu = TileEncoder::new();
        let mut external = TileEncoder::new();
        let mut packed_encoder = TileEncoder::new();
        let mut packed_cpu = TileEncoder::new();
        let mut decoder = TileDecoder::new();
        let mut previous = None;
        let mut previous_size = None;
        let mut pixels = Vec::new();
        for seq in 0..6 {
            let (width, height) = sizes[(case + usize::from(seq >= 4)) % sizes.len()];
            let size = PixelSize::new(width, height);
            let stride = width * 4 + (case % 9) as u32;
            if previous_size != Some(size) {
                pixels = vec![0; ((height - 1) * stride + width * 4) as usize];
                if case % 3 != 0 {
                    for byte in &mut pixels {
                        *byte = if case % 3 == 1 {
                            random(&mut rng) as u8
                        } else {
                            55
                        };
                    }
                }
            }
            // Unchanged captures, pixel edits, rows, and whole-frame edits.
            match seq {
                1 => {
                    for _ in 0..8 {
                        let x = random(&mut rng) as u32 % width;
                        let y = random(&mut rng) as u32 % height;
                        pixels[(y * stride + x * 4) as usize] ^= 1;
                    }
                }
                3 => pixels[..(width * 4) as usize].fill(random(&mut rng) as u8),
                5 => pixels.fill(random(&mut rng) as u8),
                _ => {}
            }
            let full = pack(size, &pixels, stride);
            let changed = bits(
                &full,
                if previous_size == Some(size) {
                    previous.as_ref()
                } else {
                    None
                },
            );
            let force = seq == 2 && case % 2 == 0;
            if seq == 3 && case % 2 == 1 {
                cpu.request_key();
                external.request_key();
                packed_encoder.request_key();
                packed_cpu.request_key();
            }
            let h = header(size, seq);
            let mut expected = Vec::new();
            let stats = cpu
                .emit(
                    cpu.scan(size, &pixels, stride).unwrap(),
                    h,
                    &pixels,
                    stride,
                    force,
                    &mut expected,
                )
                .unwrap();
            let scan = external.scan_external(size, &changed).unwrap();
            assert_eq!(scan.total() as usize, full.tiles.len());
            assert_eq!(
                scan.changed(),
                changed.iter().map(|word| word.count_ones()).sum()
            );
            let mut actual = vec![99];
            assert_eq!(
                external
                    .emit(scan, h, &pixels, stride, force, &mut actual)
                    .unwrap(),
                stats
            );
            assert_eq!(actual, expected);
            let key = force || packed_encoder.key_pending(size);
            let mut source = pack(size, &pixels, stride);
            if !key {
                for (i, tile) in source.tiles.iter_mut().enumerate() {
                    if changed[i / 32] & (1 << (i % 32)) == 0 {
                        *tile = None;
                    }
                }
            } else {
                // Fail late, after earlier records were written, and retry a scan taken before it.
                let good = packed_encoder.scan_external(size, &changed).unwrap();
                let mut missing = pack(size, &pixels, stride);
                *missing.tiles.last_mut().unwrap() = None;
                assert_eq!(
                    packed_encoder.emit_from(
                        packed_encoder.scan_external(size, &changed).unwrap(),
                        h,
                        &missing,
                        force,
                        &mut actual
                    ),
                    Err(MediaError::BadPayload)
                );
                assert!(actual.is_empty());
                assert_eq!(
                    packed_encoder
                        .emit_from(good, h, &source, force, &mut actual)
                        .unwrap(),
                    stats
                );
                assert_eq!(actual, expected);
            }
            if !key {
                assert_eq!(
                    packed_encoder
                        .emit_from(
                            packed_encoder.scan_external(size, &changed).unwrap(),
                            h,
                            &source,
                            force,
                            &mut actual
                        )
                        .unwrap(),
                    stats
                );
                assert_eq!(actual, expected);
            }
            assert_eq!(
                packed_cpu
                    .emit_from(
                        packed_cpu.scan(size, &pixels, stride).unwrap(),
                        h,
                        &source,
                        force,
                        &mut actual
                    )
                    .unwrap(),
                stats
            );
            assert_eq!(actual, expected);
            if stats.is_some() {
                decoder.apply(&actual).unwrap();
            }
            let tight: Vec<_> = (0..height)
                .flat_map(|y| {
                    pixels[(y * stride) as usize..(y * stride + width * 4) as usize]
                        .iter()
                        .copied()
                })
                .collect();
            assert_eq!(decoder.canvas(), (tight.as_slice(), size));
            previous = Some(full);
            previous_size = Some(size);
            captures += 1;
        }
    }
    assert_eq!(captures, 3000);
}

#[test]
fn malformed_and_stale_scans_preserve_state_and_output() {
    let size = PixelSize::new(65, 1);
    let pixels = vec![7; 260];
    let source = pack(size, &pixels, 260);
    for commit in [false, true] {
        let mut encoder = TileEncoder::new();
        for malformed in [vec![], vec![0, 0], vec![4], vec![u32::MAX]] {
            assert_eq!(
                encoder.scan_external(size, &malformed).unwrap_err(),
                MediaError::BadPayload
            );
        }
        assert_eq!(
            encoder
                .scan_external(PixelSize::new(0, 1), &[])
                .unwrap_err(),
            MediaError::BadSize
        );
        let stale_emit = encoder.scan_external(size, &[0]).unwrap();
        let stale_from = encoder.scan_external(size, &[0]).unwrap();
        let stale_commit = encoder.scan_external(size, &[0]).unwrap();
        let mut out = vec![99];
        let scan = encoder.scan_external(size, &[0]).unwrap();
        assert_eq!(scan.changed(), 2);
        if commit {
            encoder.commit(scan).unwrap();
        } else {
            encoder
                .emit_from(scan, header(size, 0), &source, false, &mut out)
                .unwrap();
        }
        out = vec![99];
        assert_eq!(
            encoder.emit(stale_emit, header(size, 1), &pixels, 260, false, &mut out),
            Err(MediaError::BadPayload)
        );
        assert_eq!(out, [99]);
        assert_eq!(
            encoder.emit_from(stale_from, header(size, 1), &source, false, &mut out),
            Err(MediaError::BadPayload)
        );
        assert_eq!(out, [99]);
        assert_eq!(encoder.commit(stale_commit), Err(MediaError::BadPayload));
        let good = encoder.scan_external(size, &[1]).unwrap();
        for cpu in [false, true] {
            let scan = if cpu {
                encoder.scan(size, &pixels, 260).unwrap()
            } else {
                encoder.scan_external(size, &[1]).unwrap()
            };
            assert_eq!(
                encoder.emit(scan, header(size, 1), &pixels[..259], 260, false, &mut out),
                Err(MediaError::BadPayload)
            );
            assert_eq!(out, [99]);
            let scan = if cpu {
                encoder.scan(size, &pixels, 260).unwrap()
            } else {
                encoder.scan_external(size, &[1]).unwrap()
            };
            assert_eq!(
                encoder.emit_from(
                    scan,
                    header(PixelSize::new(1, 1), 1),
                    &source,
                    false,
                    &mut out
                ),
                Err(MediaError::BadPayload)
            );
            assert_eq!(out, [99]);
            let mut bad = pack(size, &pixels, 260);
            bad.tiles[0].as_mut().unwrap().pop();
            let scan = if cpu {
                encoder.scan(size, &pixels, 260).unwrap()
            } else {
                encoder.scan_external(size, &[1]).unwrap()
            };
            assert_eq!(
                encoder.emit_from(scan, header(size, 1), &bad, false, &mut out),
                Err(MediaError::BadPayload)
            );
            assert!(out.is_empty());
            out.push(99);
        }
        let stats = encoder
            .emit_from(good, header(size, 1), &source, false, &mut out)
            .unwrap()
            .unwrap();
        assert_eq!(stats.key, commit);
        assert_eq!(stats.tiles, if commit { 2 } else { 1 });
    }
}

#[test]
fn switching_scan_kinds_with_commits_decodes() {
    let size = PixelSize::new(130, 70);
    let stride = 527;
    let mut pixels = vec![0; (69 * stride + 520) as usize];
    let mut encoder = TileEncoder::new();
    let mut decoder = TileDecoder::new();
    let mut out = Vec::new();
    for seq in 0..8 {
        pixels[seq as usize] += 1;
        let cpu = seq % 2 == 0;
        let scan = if cpu {
            encoder.scan(size, &pixels, stride).unwrap()
        } else {
            encoder.scan_external(size, &[1]).unwrap()
        };
        if cpu {
            assert_eq!(scan.changed(), scan.total());
        }
        encoder.commit(scan).unwrap();
        assert!(encoder.key_pending(size));
        let scan = if cpu {
            encoder.scan(size, &pixels, stride).unwrap()
        } else {
            encoder.scan_external(size, &[0]).unwrap()
        };
        if cpu {
            assert_eq!(scan.changed(), 0);
        }
        if seq % 3 == 0 {
            encoder
                .emit_from(
                    scan,
                    header(size, seq),
                    &pack(size, &pixels, stride),
                    false,
                    &mut out,
                )
                .unwrap();
        } else {
            encoder
                .emit(scan, header(size, seq), &pixels, stride, false, &mut out)
                .unwrap();
        }
        decoder.apply(&out).unwrap();
        let tight: Vec<_> = (0..70)
            .flat_map(|y| {
                pixels[(y * stride) as usize..(y * stride + 520) as usize]
                    .iter()
                    .copied()
            })
            .collect();
        assert_eq!(decoder.canvas().0, tight);
    }
}

#[test]
fn key_pending_matches_wire_including_periodic_unchanged_captures() {
    for cpu in [false, true] {
        let mut encoder = TileEncoder::new();
        let mut out = Vec::new();
        for seq in 0..610 {
            let size = PixelSize::new(if seq >= 604 { 2 } else { 1 }, 1);
            let pixels = vec![7; (size.width * 4) as usize];
            if seq == 602 {
                encoder.request_key();
            }
            if seq == 603 {
                encoder
                    .commit(encoder.scan_external(size, &[0]).unwrap())
                    .unwrap();
            }
            let pending = encoder.key_pending(size);
            if seq < 602 {
                assert_eq!(pending, seq % 300 == 0);
            } else if seq <= 604 {
                assert!(pending);
            }
            let force = seq == 609;
            let scan = if cpu {
                encoder.scan(size, &pixels, size.width * 4).unwrap()
            } else {
                encoder.scan_external(size, &[0]).unwrap()
            };
            let stats = if cpu {
                encoder
                    .emit(
                        scan,
                        header(size, seq),
                        &pixels,
                        size.width * 4,
                        force,
                        &mut out,
                    )
                    .unwrap()
            } else {
                encoder
                    .emit_from(
                        scan,
                        header(size, seq),
                        &pack(size, &pixels, size.width * 4),
                        force,
                        &mut out,
                    )
                    .unwrap()
            };
            if let Some(stats) = stats {
                assert_eq!(stats.key, pending || force);
                assert_eq!(read_header(&out).unwrap().key, pending || force);
            } else {
                assert!(!pending && !force);
                assert!(out.is_empty());
            }
        }
    }
}

#[test]
fn oversized_frame_clears_output_without_consuming_scan() {
    struct Repeated(Vec<u8>);
    impl TileSource for Repeated {
        fn tile(&self, _tx: u32, _ty: u32) -> Option<&[u8]> {
            Some(&self.0)
        }
    }
    let size = PixelSize::new(4096, 4096);
    let mut rng = 123;
    let noise = Repeated(
        (0..TILE * TILE * 4)
            .map(|_| random(&mut rng) as u8)
            .collect(),
    );
    let solid = Repeated(vec![0; (TILE * TILE * 4) as usize]);
    let mut encoder = TileEncoder::new();
    let bitmap = vec![0; 128];
    let good = encoder.scan_external(size, &bitmap).unwrap();
    let mut out = vec![99];
    assert_eq!(
        encoder.emit_from(
            encoder.scan_external(size, &bitmap).unwrap(),
            header(size, 0),
            &noise,
            false,
            &mut out
        ),
        Err(MediaError::TooLarge)
    );
    assert!(out.is_empty());
    assert!(encoder.key_pending(size));
    let stats = encoder
        .emit_from(good, header(size, 0), &solid, false, &mut out)
        .unwrap()
        .unwrap();
    assert!(stats.key);
    assert_eq!(stats.tiles, 4096);
}
