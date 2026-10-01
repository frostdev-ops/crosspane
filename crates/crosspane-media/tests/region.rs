use std::time::Duration;

use crosspane_media::{
    hybrid::{RegionConfig, RegionPlan, RegionScheduler, TileRect},
    tiles::{TileDecoder, TileEncoder, TilePixels, TileSource},
    wire::{
        FrameHeader, MediaError, VideoRegion, read_header, read_video, read_video_region,
        write_video, write_video_region,
    },
};
use crosspane_types::geom::PixelSize;
use proptest::{
    prelude::*,
    test_runner::{Config, RngAlgorithm, TestRng, TestRunner},
};

fn header(size: PixelSize, seq: u64) -> FrameHeader {
    FrameHeader {
        projection: 1,
        seq,
        key: false,
        captured_ns: seq,
        width: size.width,
        height: size.height,
    }
}

fn bitmap(size: PixelSize, tiles: &[(u32, u32)]) -> Vec<u32> {
    let nx = size.width.div_ceil(64);
    let mut bits = vec![0; (nx * size.height.div_ceil(64)).div_ceil(32) as usize];
    for &(x, y) in tiles {
        let i = (y * nx + x) as usize;
        bits[i / 32] |= 1 << (i % 32);
    }
    bits
}

fn paint(pixels: &mut [u8], size: PixelSize, tx: u32, ty: u32, value: u8) {
    for y in ty * 64..((ty + 1) * 64).min(size.height) {
        for x in tx * 64..((tx + 1) * 64).min(size.width) {
            let i = ((y * size.width + x) * 4) as usize;
            pixels[i..i + 4].copy_from_slice(&[value, x as u8, y as u8, 255]);
        }
    }
}

struct Receiver {
    decoder: TileDecoder,
    video: Vec<bool>,
    shown: Vec<u8>,
}

#[allow(clippy::unwrap_used)] // Receiver is a test-only codec oracle.
impl Receiver {
    fn new(size: PixelSize) -> Self {
        Self {
            decoder: TileDecoder::new(),
            shown: vec![0; (size.width * size.height * 4) as usize],
            video: vec![false; (size.width.div_ceil(64) * size.height.div_ceil(64)) as usize],
        }
    }

    // A video picture selects video in its rectangle. A later lossless record wins.
    fn capture(
        &mut self,
        size: PixelSize,
        source: &[u8],
        region: Option<TileRect>,
        tiles: &[u8],
        seq: u64,
    ) -> Vec<bool> {
        let nx = size.width.div_ceil(64);
        if let Some(rect) = region {
            let mut wire = Vec::new();
            write_video_region(header(size, seq), rect.region(size), b"dummy AU", &mut wire)
                .unwrap();
            let (h, parsed, au) = read_video_region(&wire).unwrap();
            assert_eq!(h, header(size, seq));
            assert_eq!(parsed, Some(rect.region(size)));
            assert_eq!(au, b"dummy AU");
            let picture = parsed.unwrap();
            for y in picture.y..picture.y + picture.height {
                let start = ((y * size.width + picture.x) * 4) as usize;
                let end = start + (picture.width * 4) as usize;
                self.shown[start..end].copy_from_slice(&source[start..end]);
            }
            for (i, video) in self.video.iter_mut().enumerate() {
                if rect.contains(i as u32 % nx, i as u32 / nx) {
                    *video = true;
                }
            }
        }
        let mut sent = vec![false; self.video.len()];
        if !tiles.is_empty() {
            let (_, rects) = self.decoder.apply(tiles).unwrap();
            for rect in rects {
                let i = (rect.min.y as u32 / 64 * nx + rect.min.x as u32 / 64) as usize;
                self.video[i] = false;
                sent[i] = true;
                let canvas = self.decoder.canvas().0;
                for y in rect.min.y as u32..rect.max.y as u32 {
                    let start = ((y * size.width + rect.min.x as u32) * 4) as usize;
                    let end = start + ((rect.max.x - rect.min.x) * 4) as usize;
                    self.shown[start..end].copy_from_slice(&canvas[start..end]);
                }
            }
        }
        let (canvas, canvas_size) = self.decoder.canvas();
        assert_eq!(canvas_size, size);
        for (i, video) in self.video.iter().enumerate() {
            let tx = i as u32 % nx;
            let ty = i as u32 / nx;
            if *video {
                assert!(
                    region.is_some_and(|rect| rect.contains(tx, ty)),
                    "stale video outside latest region"
                );
                // The dummy video decoder supplies exactly the source pixels in the region.
            } else {
                for y in ty * 64..((ty + 1) * 64).min(size.height) {
                    let start = ((y * size.width + tx * 64) * 4) as usize;
                    let end = start + (64.min(size.width - tx * 64) * 4) as usize;
                    assert_eq!(
                        &canvas[start..end],
                        &source[start..end],
                        "canvas tile {tx},{ty}"
                    );
                }
            }
        }
        assert_eq!(self.shown, source);
        sent
    }
}

#[test]
fn scripted_codec_sequence() {
    let size = PixelSize::new(640, 384);
    let mut source = vec![0; (size.width * size.height * 4) as usize];
    for ty in 0..6 {
        for tx in 0..10 {
            paint(&mut source, size, tx, ty, 42);
        }
    }
    let mut encoder = TileEncoder::new();
    let mut scheduler = RegionScheduler::new(RegionConfig::default());
    let mut receiver = Receiver::new(size);
    // Static page, centre animation, moving, growing, second box joining, shrinking, stopping.
    let sequence: Vec<(u64, Vec<(u32, u32)>)> = vec![
        (0, vec![]),
        (100, vec![(4, 2)]),
        (200, vec![(4, 2)]),
        (300, vec![(4, 2)]),
        (400, vec![(4, 2)]),
        (500, vec![(4, 2), (5, 2)]),
        (600, vec![(5, 2)]),
        (700, vec![(5, 2)]),
        (800, vec![(5, 2), (6, 2)]),
        (900, vec![(5, 2), (6, 2)]),
        (1000, vec![(5, 2), (6, 2)]),
        (1100, vec![(5, 2), (6, 2), (1, 4)]),
        (1200, vec![(5, 2), (6, 2), (1, 4)]),
        (1300, vec![(5, 2), (6, 2), (1, 4)]),
        (1400, vec![(5, 2)]),
        (1700, vec![(5, 2)]),
        (2000, vec![(5, 2)]),
        (2300, vec![(5, 2)]),
        (2400, vec![(5, 2)]),
        (2600, vec![(5, 2)]),
        (2700, vec![(5, 2)]),
        (3100, vec![]),
        (3200, vec![]),
    ];
    let mut counts = Vec::new();
    let mut regions = Vec::new();
    for (seq, (ms, changed)) in sequence.into_iter().enumerate() {
        for (x, y) in changed {
            paint(&mut source, size, x, y, seq as u8);
        }
        let scan = encoder.scan(size, &source, size.width * 4).unwrap();
        let region =
            match scheduler.plan(10, 6, &scan.changed_bits(), Duration::from_millis(ms), true) {
                RegionPlan::Tiles => None,
                RegionPlan::Video { region, .. } => Some(region),
            };
        let mut out = Vec::new();
        let stats = encoder
            .emit_region(
                scan,
                header(size, seq as u64),
                TilePixels::Strided {
                    pixels: &source,
                    stride: size.width * 4,
                },
                region,
                false,
                &mut out,
            )
            .unwrap();
        receiver.capture(size, &source, region, &out, seq as u64);
        counts.push(stats.map_or(0, |stats| stats.tiles));
        regions.push(region.map(|r| (r.x, r.y, r.width, r.height)));
    }
    assert!(receiver.video.iter().all(|video| !video));
    assert_eq!(receiver.decoder.canvas().0, source);
    assert_eq!(counts[0], 60);
    assert!(counts[3..counts.len() - 2].iter().all(|&count| count < 60));
    println!("tiles per capture: {counts:?}; regions (x,y,w,h) in tiles: {regions:?}");
}

#[test]
fn seeded_properties_320_sequences() {
    let config = Config {
        cases: 320,
        failure_persistence: None,
        ..Config::default()
    };
    let rng = TestRng::from_seed(RngAlgorithm::ChaCha, &[32; 32]);
    let mut runner = TestRunner::new_with_rng(config, rng);
    let strategy =
        prop::collection::vec((any::<u32>(), 1_u64..450, any::<bool>(), 0_u8..20), 25..45);
    runner
        .run(&strategy, |steps| {
            let size = PixelSize::new(321, 193); // Clipped edge tiles; 6 × 4 grid.
            let mut source = vec![0; (size.width * size.height * 4) as usize];
            let mut encoder = TileEncoder::new();
            let mut scheduler = RegionScheduler::new(RegionConfig::default());
            let mut receiver = Receiver::new(size);
            let mut now = Duration::ZERO;
            let mut previous: Option<TileRect> = None;
            let mut grown_at = Duration::ZERO;
            for (seq, (mask, delta, force, event)) in steps.into_iter().enumerate() {
                now += Duration::from_millis(delta);
                for i in 0..24 {
                    if mask & (1 << i) != 0 {
                        paint(&mut source, size, i % 6, i / 6, seq as u8 + 1);
                    }
                }
                let scan = encoder.scan(size, &source, size.width * 4).unwrap();
                let bits = scan.changed_bits();
                prop_assert_eq!(
                    bits.iter().map(|v| v.count_ones()).sum::<u32>(),
                    scan.changed()
                );
                if event == 0 {
                    scheduler.video_failed(now);
                }
                let region = match scheduler.plan(6, 4, &bits, now, event != 1) {
                    RegionPlan::Tiles => None,
                    RegionPlan::Video { region, key } => {
                        if let Some(old) = previous {
                            if region.width < old.width || region.height < old.height {
                                prop_assert!(
                                    now.saturating_sub(grown_at) >= Duration::from_secs(1)
                                );
                            }
                            if region.width > old.width || region.height > old.height {
                                grown_at = now;
                            }
                            prop_assert_eq!(
                                key,
                                (region.width, region.height) != (old.width, old.height)
                            );
                        } else {
                            grown_at = now;
                            prop_assert!(key);
                        }
                        Some(region)
                    }
                };
                previous = region;
                let mut out = Vec::new();
                let stats = encoder
                    .emit_region(
                        scan,
                        header(size, seq as u64),
                        TilePixels::Strided {
                            pixels: &source,
                            stride: size.width * 4,
                        },
                        region,
                        force,
                        &mut out,
                    )
                    .unwrap();
                let sent = receiver.capture(size, &source, region, &out, seq as u64);
                for (i, &sent) in sent.iter().enumerate() {
                    if !region.is_some_and(|r| r.contains(i as u32 % 6, i as u32 / 6))
                        && bits[0] & (1 << i) != 0
                    {
                        prop_assert!(sent, "changed outside tile missing");
                    }
                }
                if stats.is_some_and(|s| s.key) {
                    prop_assert!(sent.iter().all(|sent| *sent));
                    // A subsequent unchanged, unexcluded emit must find no stale tiles.
                    let scan = encoder.scan(size, &source, size.width * 4).unwrap();
                    prop_assert!(
                        encoder
                            .emit(
                                scan,
                                header(size, seq as u64),
                                &source,
                                size.width * 4,
                                false,
                                &mut out
                            )
                            .unwrap()
                            .is_none()
                    );
                }
            }
            let scan = encoder.scan(size, &source, size.width * 4).unwrap();
            let mut out = Vec::new();
            encoder
                .emit(
                    scan,
                    header(size, 100),
                    &source,
                    size.width * 4,
                    false,
                    &mut out,
                )
                .unwrap();
            receiver.capture(size, &source, None, &out, 100);
            prop_assert_eq!(receiver.decoder.canvas().0, source);
            Ok(())
        })
        .unwrap();
}

#[test]
fn scheduler_thresholds_hysteresis_expiry_and_retry() {
    let size = PixelSize::new(640, 384);
    let mut scheduler = RegionScheduler::new(RegionConfig::default());
    let mut plan = |ms, tiles: &[(u32, u32)]| {
        scheduler.plan(10, 6, &bitmap(size, tiles), Duration::from_millis(ms), true)
    };
    assert_eq!(plan(0, &[(4, 2)]), RegionPlan::Tiles);
    // Changes accumulate across short quiet gaps, rather than requiring consecutive captures.
    assert_eq!(plan(100, &[]), RegionPlan::Tiles);
    assert_eq!(plan(200, &[(4, 2)]), RegionPlan::Tiles);
    let centre = TileRect {
        x: 3,
        y: 1,
        width: 3,
        height: 3,
    };
    assert_eq!(
        plan(300, &[(4, 2)]),
        RegionPlan::Video {
            region: centre,
            key: true
        }
    );
    assert_eq!(
        plan(699, &[]),
        RegionPlan::Video {
            region: centre,
            key: false
        }
    );
    assert_eq!(plan(700, &[]), RegionPlan::Tiles);
    for ms in [800, 900, 1000] {
        plan(ms, &[(4, 2), (7, 2)]);
    }
    let large = TileRect {
        x: 3,
        y: 1,
        width: 6,
        height: 3,
    };
    assert_eq!(
        plan(1100, &[(4, 2)]),
        RegionPlan::Video {
            region: large,
            key: false
        }
    );
    // Other tile expires at 1400; shrinking may begin then, but only completes at 2400.
    for ms in [1400, 1700, 2000, 2300] {
        assert_eq!(
            plan(ms, &[(4, 2)]),
            RegionPlan::Video {
                region: large,
                key: false
            }
        );
    }
    assert_eq!(
        plan(2400, &[(4, 2)]),
        RegionPlan::Video {
            region: centre,
            key: true
        }
    );
    scheduler.video_failed(Duration::from_millis(2400));
    assert_eq!(scheduler.region(), None);
    for ms in (2500..12400).step_by(100) {
        assert_eq!(
            scheduler.plan(
                10,
                6,
                &bitmap(size, &[(4, 2)]),
                Duration::from_millis(ms),
                true
            ),
            RegionPlan::Tiles
        );
    }
    assert_eq!(
        scheduler.plan(
            10,
            6,
            &bitmap(size, &[(4, 2)]),
            Duration::from_millis(12400),
            true
        ),
        RegionPlan::Video {
            region: centre,
            key: true
        }
    );
    assert_eq!(
        scheduler.plan(
            10,
            6,
            &bitmap(size, &[(4, 2)]),
            Duration::from_millis(12401),
            false
        ),
        RegionPlan::Tiles
    );
    assert_eq!(
        scheduler.plan(1, 1, &[1], Duration::ZERO, true),
        RegionPlan::Tiles
    );
    assert_eq!(
        centre.region(PixelSize::new(321, 193)),
        VideoRegion {
            x: 192,
            y: 64,
            width: 129,
            height: 129
        }
    );
    assert!(
        !TileRect {
            x: u32::MAX,
            y: 0,
            width: 2,
            height: 1
        }
        .contains(0, 0)
    );
}

#[test]
fn wire_round_trip_old_frames_and_rejections() {
    let size = PixelSize::new(129, 193);
    let h = FrameHeader {
        key: true,
        ..header(size, 7)
    };
    let region = VideoRegion {
        x: 64,
        y: 64,
        width: 65,
        height: 129,
    };
    let mut out = Vec::new();
    write_video_region(h, region, b"AU", &mut out).unwrap();
    assert_eq!(out[5], 5);
    assert_eq!(&out[44..48], &18_u32.to_le_bytes());
    assert_eq!(
        read_video_region(&out).unwrap(),
        (h, Some(region), b"AU".as_slice())
    );
    assert_eq!(read_header(&out).unwrap(), h);
    assert_eq!(read_video(&out), Err(MediaError::BadReserved));
    for bad in [
        VideoRegion { x: 1, ..region },
        VideoRegion { y: 1, ..region },
        VideoRegion { width: 0, ..region },
        VideoRegion {
            height: 0,
            ..region
        },
        VideoRegion {
            width: 64 + 64,
            ..region
        },
        VideoRegion {
            height: 64 + 128,
            ..region
        },
        VideoRegion {
            width: 63,
            ..region
        },
        VideoRegion {
            height: 63,
            ..region
        },
        VideoRegion {
            x: u32::MAX,
            width: 2,
            ..region
        },
    ] {
        assert_eq!(
            write_video_region(h, bad, b"AU", &mut out),
            Err(MediaError::BadSize)
        );
        write_video_region(h, region, b"AU", &mut out).unwrap();
        for (i, value) in [bad.x, bad.y, bad.width, bad.height]
            .into_iter()
            .enumerate()
        {
            out[48 + i * 4..52 + i * 4].copy_from_slice(&value.to_le_bytes());
        }
        assert_eq!(read_video_region(&out), Err(MediaError::BadSize));
    }
    write_video_region(h, region, b"AU", &mut out).unwrap();
    for len in 0..out.len() {
        assert!(read_video_region(&out[..len]).is_err());
    }
    out.push(0);
    assert_eq!(read_video_region(&out), Err(MediaError::Trailing));
    write_video(h, b"old", &mut out).unwrap();
    assert_eq!(read_video(&out).unwrap(), (h, b"old".as_slice()));
    assert_eq!(
        read_video_region(&out).unwrap(),
        (h, None, b"old".as_slice())
    );
    out[5] |= 2;
    assert_eq!(read_video_region(&out), Err(MediaError::BadReserved));
    out[6] = 0;
    out[5] = 4;
    assert_eq!(read_header(&out), Err(MediaError::BadReserved));
    out[6] = 2;
    assert_eq!(read_header(&out), Err(MediaError::BadReserved));
}

struct Packed {
    tiles: Vec<Vec<u8>>,
    nx: u32,
}
impl TileSource for Packed {
    fn tile(&self, tx: u32, ty: u32) -> Option<&[u8]> {
        self.tiles
            .get((ty * self.nx + tx) as usize)
            .map(Vec::as_slice)
    }
}

#[test]
fn external_packed_stale_refresh_errors_commit_and_keys() {
    let size = PixelSize::new(192, 64);
    let packed = Packed {
        tiles: vec![vec![17; 64 * 64 * 4]; 3],
        nx: 3,
    };
    let mut encoder = TileEncoder::new();
    let mut out = Vec::new();
    let scan = encoder.scan_external(size, &[0]).unwrap();
    assert_eq!(scan.changed_bits(), vec![7]);
    encoder
        .emit_from(scan, header(size, 0), &packed, false, &mut out)
        .unwrap();
    let rect = TileRect {
        x: 1,
        y: 0,
        width: 1,
        height: 1,
    };
    let scan = encoder.scan_external(size, &[0]).unwrap();
    assert!(
        encoder
            .emit_region(
                scan,
                header(size, 1),
                TilePixels::Packed(&packed),
                Some(rect),
                false,
                &mut out
            )
            .unwrap()
            .is_none()
    );
    assert!(!encoder.key_pending(size));
    let missing = Packed {
        tiles: vec![],
        nx: 3,
    };
    let scan = encoder.scan_external(size, &[0]).unwrap();
    assert_eq!(
        encoder.emit_region(
            scan,
            header(size, 2),
            TilePixels::Packed(&missing),
            None,
            false,
            &mut out
        ),
        Err(MediaError::BadPayload)
    );
    assert!(out.is_empty());
    let scan = encoder.scan_external(size, &[0]).unwrap();
    assert_eq!(
        encoder
            .emit_from(scan, header(size, 3), &packed, false, &mut out)
            .unwrap()
            .unwrap()
            .tiles,
        1
    );
    let scan = encoder.scan_external(size, &[0]).unwrap();
    assert!(
        encoder
            .emit_region(
                scan,
                header(size, 4),
                TilePixels::Packed(&packed),
                Some(rect),
                false,
                &mut out
            )
            .unwrap()
            .is_none()
    );
    let scan = encoder.scan_external(size, &[0]).unwrap();
    encoder.commit(scan).unwrap();
    assert!(encoder.key_pending(size));
    let scan = encoder.scan_external(size, &[0]).unwrap();
    let stats = encoder
        .emit_region(
            scan,
            header(size, 5),
            TilePixels::Packed(&packed),
            Some(rect),
            false,
            &mut out,
        )
        .unwrap()
        .unwrap();
    assert!(stats.key);
    assert_eq!(stats.tiles, 3);
    let scan = encoder.scan_external(size, &[0]).unwrap();
    assert!(
        encoder
            .emit_from(scan, header(size, 6), &packed, false, &mut out)
            .unwrap()
            .is_none()
    );
    let small = PixelSize::new(64, 64);
    let scan = encoder.scan_external(small, &[0]).unwrap();
    assert_eq!(
        encoder
            .emit_region(
                scan,
                header(small, 7),
                TilePixels::Packed(&packed),
                Some(rect),
                false,
                &mut out
            )
            .unwrap()
            .unwrap()
            .tiles,
        1
    );
    let cover = TileRect {
        x: 0,
        y: 0,
        width: 1,
        height: 1,
    };
    for seq in 0..299 {
        let scan = encoder.scan_external(small, &[0]).unwrap();
        assert!(
            encoder
                .emit_region(
                    scan,
                    header(small, seq),
                    TilePixels::Packed(&packed),
                    Some(cover),
                    false,
                    &mut out
                )
                .unwrap()
                .is_none()
        );
    }
    assert!(encoder.key_pending(small));
    let scan = encoder.scan_external(small, &[0]).unwrap();
    let periodic = encoder
        .emit_region(
            scan,
            header(small, 300),
            TilePixels::Packed(&packed),
            Some(cover),
            false,
            &mut out,
        )
        .unwrap()
        .unwrap();
    assert!(periodic.key);
    assert_eq!(periodic.tiles, 1);
    let scan = encoder.scan_external(small, &[0]).unwrap();
    encoder
        .emit_region(
            scan,
            header(small, 301),
            TilePixels::Packed(&packed),
            Some(cover),
            false,
            &mut out,
        )
        .unwrap();
    encoder.request_key();
    let scan = encoder.scan_external(small, &[0]).unwrap();
    assert!(
        encoder
            .emit_region(
                scan,
                header(small, 302),
                TilePixels::Packed(&packed),
                Some(cover),
                false,
                &mut out
            )
            .unwrap()
            .unwrap()
            .key
    );
}
