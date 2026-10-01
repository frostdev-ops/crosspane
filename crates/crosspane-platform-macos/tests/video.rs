#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;
use std::time::Instant;

use crosspane_media::codec::{CodecError, VideoCodecs, VideoDecoder};
use crosspane_media::picture::{Nv12, YuvMatrix, nv12_to_bgra};
use crosspane_platform_macos::video::VtCodecs;
use crosspane_types::geom::PixelSize;

fn available<T>(result: Result<T, CodecError>) -> Option<T> {
    match result {
        Ok(codec) => Some(codec),
        Err(CodecError::Unavailable(reason)) => {
            eprintln!("skipped: VideoToolbox unavailable: {reason}");
            None
        }
        Err(error) => panic!("VideoToolbox factory: {error}"),
    }
}

fn synthetic(size: PixelSize, frame: u32) -> (Vec<u8>, u32) {
    let stride = size.width * 4 + 12; // Exercise copying rows with source padding.
    let mut pixels = vec![0xa5; stride as usize * size.height as usize];
    let box_width = (size.width / 5).max(2) & !1;
    let box_height = (size.height / 5).max(2) & !1;
    let left = (frame * 6 % (size.width - box_width)) & !1;
    let top = (frame * 4 % (size.height - box_height)) & !1;
    for y in 0..size.height {
        for x in 0..size.width {
            let pixel = &mut pixels[(y * stride + x * 4) as usize..][..4];
            pixel.copy_from_slice(&[
                (x * 200 / size.width + 20) as u8,
                ((x * 100 / size.width) + (y * 100 / size.height) + 20) as u8,
                (y * 200 / size.height + 20) as u8,
                255,
            ]);
            if x >= left && x < left + box_width && y >= top && y < top + box_height {
                pixel.copy_from_slice(&[48, 150, 208, 255]);
            }
        }
    }
    (pixels, stride)
}

fn nals(data: &[u8]) -> Vec<&[u8]> {
    let starts: Vec<_> = data
        .windows(3)
        .enumerate()
        .filter_map(|(i, bytes)| (bytes == [0, 0, 1]).then_some(i))
        .collect();
    assert!(!starts.is_empty(), "Annex B start codes required");
    starts
        .iter()
        .enumerate()
        .map(|(i, start)| {
            let mut end = starts.get(i + 1).copied().unwrap_or(data.len());
            while end > start + 3 && data[end - 1] == 0 {
                end -= 1;
            }
            assert!(end > start + 3, "nonempty NAL");
            &data[start + 3..end]
        })
        .collect()
}

fn assert_key(data: &[u8], key: bool) {
    let types: Vec<_> = nals(data).iter().map(|nal| nal[0] & 31).collect();
    if key {
        assert!(
            types.contains(&7) && types.contains(&8) && types.contains(&5),
            "IDR needs SPS, PPS and IDR slices: {types:?}"
        );
        let sps = types.iter().position(|&kind| kind == 7).unwrap();
        let pps = types.iter().position(|&kind| kind == 8).unwrap();
        let idr = types.iter().position(|&kind| kind == 5).unwrap();
        assert!(sps < pps && pps < idr);
    } else {
        assert!(
            types.contains(&1) && !types.contains(&5),
            "expected non-IDR: {types:?}"
        );
    }
}

fn psnr(
    source: &[u8],
    source_stride: u32,
    decoded: &[u8],
    coded: PixelSize,
    crop: PixelSize,
) -> f64 {
    let mut square_error = 0u64;
    for y in 0..crop.height {
        for x in 0..crop.width {
            for channel in 0..3 {
                let a = source[(y * source_stride + x * 4 + channel) as usize];
                let b = decoded[((y * coded.width + x) * 4 + channel) as usize];
                square_error += u64::from(a.abs_diff(b)).pow(2);
            }
        }
    }
    let mse = square_error as f64 / (f64::from(crop.width) * f64::from(crop.height) * 3.0);
    10.0 * (255.0f64.powi(2) / mse).log10()
}

fn round_trip(size: PixelSize) {
    let codecs = VtCodecs::new();
    let Some(mut encoder) = available(codecs.encoder(size, 8_000_000, 30)) else {
        return;
    };
    let Some(mut decoder) = available(codecs.decoder()) else {
        return;
    };
    let expected = PixelSize::new((size.width + 1) & !1, (size.height + 1) & !1);
    let mut encoded =
        Vec::with_capacity(expected.width as usize * expected.height as usize * 4 + 1024);
    let mut decoded = Vec::with_capacity(expected.width as usize * expected.height as usize * 4);
    encoded.extend_from_slice(&[0xff; 100]);
    decoded.extend_from_slice(&[0xff; 100]);
    let encoded_pointer = encoded.as_ptr();
    let decoded_pointer = decoded.as_ptr();
    let mut encode_time = 0.0;
    let mut decode_time = 0.0;
    let mut bytes = 0usize;
    let mut lowest_psnr = f64::INFINITY;
    for frame in 0..30 {
        let (pixels, stride) = synthetic(size, frame);
        let force_key = frame == 10 || frame == 20;
        let start = Instant::now();
        let result = encoder
            .encode(&pixels, stride, size, force_key, &mut encoded)
            .unwrap();
        encode_time += start.elapsed().as_secs_f64();
        assert_eq!(
            encoded.as_ptr(),
            encoded_pointer,
            "reuse the caller's encode allocation"
        );
        assert_eq!(result.key, frame == 0 || force_key);
        assert_key(&encoded, result.key);
        assert!(!encoded.is_empty());
        bytes += encoded.len();
        let start = Instant::now();
        let coded = decoder.decode(&encoded, &mut decoded).unwrap();
        decode_time += start.elapsed().as_secs_f64();
        assert_eq!(
            decoded.as_ptr(),
            decoded_pointer,
            "reuse the caller's decode allocation"
        );
        assert_eq!(coded, expected);
        assert_eq!(
            decoded.len(),
            coded.width as usize * coded.height as usize * 4
        );
        let quality = psnr(&pixels, stride, &decoded, coded, size);
        lowest_psnr = lowest_psnr.min(quality);
        assert!(
            quality >= 30.0,
            "{size:?} frame {frame}: PSNR {quality:.2} dB"
        );
        // Every IDR must also decode in an entirely fresh decoder.
        if result.key {
            let Some(mut fresh) = available(codecs.decoder()) else {
                return;
            };
            let mut independent = Vec::new();
            assert_eq!(fresh.decode(&encoded, &mut independent).unwrap(), expected);
            assert_eq!(independent, decoded);
        }
    }
    assert!(encoder.name().contains("hardware"));
    assert!(decoder.name().contains("hardware"));
    eprintln!(
        "{}x{} (coded {}x{}): 30 frames; encode {:.3} ms, decode {:.3} ms, {:.1} bytes/frame, min PSNR {:.2} dB; encoder={}, decoder={}",
        size.width,
        size.height,
        expected.width,
        expected.height,
        encode_time * 1000.0 / 30.0,
        decode_time * 1000.0 / 30.0,
        bytes as f64 / 30.0,
        lowest_psnr,
        encoder.name(),
        decoder.name()
    );
}

#[test]
fn round_trip_320x240() {
    round_trip(PixelSize::new(320, 240));
}

#[test]
fn round_trip_1280x720() {
    round_trip(PixelSize::new(1280, 720));
}

#[test]
fn round_trip_odd_101x75() {
    round_trip(PixelSize::new(101, 75));
}

#[test]
fn size_change_and_non_idr_recovery() {
    let codecs = VtCodecs::new();
    let size = PixelSize::new(320, 240);
    let Some(mut encoder) = available(codecs.encoder(size, 8_000_000, 30)) else {
        return;
    };
    let Some(mut decoder) = available(codecs.decoder()) else {
        return;
    };
    let mut packet = Vec::new();
    let mut decoded = Vec::new();
    let (pixels, stride) = synthetic(size, 0);
    assert!(
        encoder
            .encode(&pixels, stride, size, false, &mut packet)
            .unwrap()
            .key
    );
    // Deliberately omit the initial IDR.
    let (pixels, stride) = synthetic(size, 1);
    assert!(
        !encoder
            .encode(&pixels, stride, size, false, &mut packet)
            .unwrap()
            .key
    );
    assert!(decoder.decode(&packet, &mut decoded).is_err());
    assert!(
        encoder
            .encode(&pixels, stride, size, true, &mut packet)
            .unwrap()
            .key
    );
    assert_key(&packet, true);
    assert_eq!(decoder.decode(&packet, &mut decoded).unwrap(), size);
    // Changing SPS/PPS must replace the decoder session too, including odd-size padding.
    for size in [
        PixelSize::new(1280, 720),
        PixelSize::new(101, 75),
        PixelSize::new(102, 76),
    ] {
        let (pixels, stride) = synthetic(size, 2);
        assert!(
            encoder
                .encode(&pixels, stride, size, false, &mut packet)
                .unwrap()
                .key
        );
        assert_key(&packet, true);
        let coded = PixelSize::new((size.width + 1) & !1, (size.height + 1) & !1);
        assert_eq!(decoder.decode(&packet, &mut decoded).unwrap(), coded);
        assert!(psnr(&pixels, stride, &decoded, coded, size) >= 30.0);
    }
}

fn detailed_motion(size: PixelSize, frame: u32) -> (Vec<u8>, u32) {
    let (mut pixels, stride) = synthetic(size, frame);
    // The low-detail gradient needs less than 1 Mbit/s at either setting. A large moving textured
    // colour box gives rate control sufficient detail to distinguish the requested targets.
    let left = frame * 8 % (size.width / 4);
    let top = frame * 4 % (size.height / 4);
    for y in top..top + size.height * 3 / 4 {
        for x in left..left + size.width * 3 / 4 {
            let mut noise = (x / 2 - left / 2).wrapping_mul(0x45d9f3b)
                ^ (y / 2 - top / 2).wrapping_mul(0x119de1f3);
            noise ^= noise >> 16;
            let pixel = &mut pixels[(y * stride + x * 4) as usize..][..4];
            let detail = (noise & 127) as u8;
            pixel.copy_from_slice(&[
                detail,
                detail.saturating_add(32),
                detail.saturating_add(64),
                255,
            ]);
        }
    }
    (pixels, stride)
}

#[test]
fn set_bitrate_reduces_average_frame_size() {
    let codecs = VtCodecs::new();
    let size = PixelSize::new(1280, 720);
    let Some(mut encoder) = available(codecs.encoder(size, 8_000_000, 30)) else {
        return;
    };
    let mut packet = Vec::new();
    let mut averages = Vec::new();
    for bitrate in [8_000_000, 1_000_000] {
        encoder.set_bitrate(bitrate);
        let mut bytes = 0usize;
        for frame in 0..30 {
            let (pixels, stride) = detailed_motion(size, frame);
            encoder
                .encode(&pixels, stride, size, frame == 0, &mut packet)
                .unwrap_or_else(|error| panic!("bitrate {bitrate}, frame {frame}: {error}"));
            bytes += packet.len();
        }
        averages.push(bytes as f64 / 30.0);
    }
    eprintln!(
        "set_bitrate 1280x720: 8 Mbit/s {:.1} bytes/frame -> 1 Mbit/s {:.1} bytes/frame ({:.1}% drop)",
        averages[0],
        averages[1],
        100.0 * (1.0 - averages[1] / averages[0])
    );
    assert!(
        averages[1] <= averages[0] * 0.7,
        "bitrate response: {averages:?}"
    );
}

#[test]
fn ffmpeg_annex_b_interop() {
    let output = match Command::new("ffmpeg")
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=30",
            "-frames:v",
            "10",
            "-c:v",
            "libx264",
            "-tune",
            "zerolatency",
            "-bf",
            "0",
            "-f",
            "h264",
            "-",
        ])
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipped: ffmpeg is not installed");
            return;
        }
        Err(error) => panic!("launch ffmpeg: {error}"),
    };
    assert!(
        output.status.success(),
        "ffmpeg: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut units = Vec::new();
    let mut unit = Vec::new();
    let mut has_slice = false;
    for nal in nals(&output.stdout) {
        let slice = matches!(nal[0] & 31, 1 | 5);
        // first_mb_in_slice is the first unsigned Exp-Golomb value. Its zero encoding is a
        // leading one bit; only that first slice starts a new picture in this x264 stream.
        if slice && nal[1] & 0x80 != 0 && has_slice {
            units.push(std::mem::take(&mut unit));
            has_slice = false;
        }
        unit.extend_from_slice(&[0, 0, 0, 1]);
        unit.extend_from_slice(nal);
        has_slice |= slice;
    }
    if has_slice {
        units.push(unit);
    }
    assert_eq!(units.len(), 10, "one access unit per x264 picture");
    let Some(mut decoder) = available(VtCodecs::new().decoder()) else {
        return;
    };
    let mut pixels = Vec::new();
    for (index, unit) in units.iter().enumerate() {
        assert_eq!(
            decoder
                .decode(unit, &mut pixels)
                .unwrap_or_else(|error| panic!("ffmpeg frame {index}: {error}")),
            PixelSize::new(320, 240)
        );
        assert_eq!(pixels.len(), 320 * 240 * 4);
        assert!(
            pixels
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| pixel[3] == 255)
        );
    }
    eprintln!(
        "ffmpeg interop: {} frames; decoder={}",
        units.len(),
        decoder.name()
    );
    let Some(mut missing_reference) = available(VtCodecs::new().decoder()) else {
        return;
    };
    missing_reference.decode(&units[0], &mut pixels).unwrap();
    let result = missing_reference.decode(&units[2], &mut pixels);
    assert!(
        result.is_err(),
        "frame 2 depends on missing frame 1: {result:?}"
    );
    missing_reference.decode(&units[0], &mut pixels).unwrap();
}

const PATCHES: [[u8; 4]; 12] = [
    [0, 0, 255, 255],
    [0, 255, 0, 255],
    [255, 0, 0, 255],
    [255, 255, 0, 255],
    [255, 0, 255, 255],
    [0, 255, 255, 255],
    [255, 255, 255, 255],
    [0, 0, 0, 255],
    [128, 128, 128, 255],
    [48, 150, 208, 255],
    [160, 80, 32, 255],
    [80, 176, 112, 255],
];

#[test]
fn nv12_colour_round_trip_and_pool_resize() {
    let codecs = VtCodecs::new();
    let Some(mut encoder) =
        available(codecs.encoder_with_fallback(PixelSize::new(256, 256), 20_000_000, 30, false))
    else {
        return;
    };
    let Some(mut decoder) = available(codecs.nv12_decoder()) else {
        return;
    };
    let Some(mut bgra_decoder) = available(codecs.decoder()) else {
        return;
    };
    let mut packet = Vec::new();
    let mut picture = Nv12::default();
    let mut converted = Vec::new();
    let mut bgra = Vec::new();
    for size in [PixelSize::new(256, 256), PixelSize::new(250, 142)] {
        // Four columns of 64x64 blocks. At the smaller size, the final row/column is
        // clipped; its centre is the centre of the visible part, away from chroma edges.
        let mut pixels = vec![0; (size.width * size.height * 4) as usize];
        for y in 0..size.height {
            for x in 0..size.width {
                let colour = PATCHES[((y / 64 * 4 + x / 64) % 12) as usize];
                pixels[((y * size.width + x) * 4) as usize..][..4].copy_from_slice(&colour);
            }
        }
        let mut maximum = [0u8; 3];
        let mut agreement = [0u8; 3];
        for frame in 0..3 {
            let key = encoder
                .encode(&pixels, size.width * 4, size, false, &mut packet)
                .unwrap()
                .key;
            assert_eq!(key, frame == 0);
            decoder.decode_nv12(&packet, &mut picture).unwrap();
            picture.validate().unwrap();
            assert_eq!(picture.size, size);
            assert_eq!(picture.colour.matrix, YuvMatrix::Bt709);
            nv12_to_bgra(&picture, size, &mut converted).unwrap();
            assert_eq!(bgra_decoder.decode(&packet, &mut bgra).unwrap(), size);
            for (index, colour) in PATCHES.iter().enumerate() {
                let left = index as u32 % 4 * 64;
                let top = index as u32 / 4 * 64;
                let x = left + (size.width - left).min(64) / 2;
                let y = top + (size.height - top).min(64) / 2;
                let at = ((y * size.width + x) * 4) as usize;
                for channel in 0..3 {
                    maximum[channel] =
                        maximum[channel].max(converted[at + channel].abs_diff(colour[channel]));
                    agreement[channel] = agreement[channel]
                        .max(converted[at + channel].abs_diff(bgra[at + channel]));
                }
            }
        }
        eprintln!(
            "{size:?}: NV12 {:?}, strides Y={} UV={}, max source errors BGR={maximum:?}, BGRA agreement={agreement:?}",
            picture.colour, picture.y_stride, picture.uv_stride
        );
        assert!(
            maximum.iter().all(|&error| error <= 8),
            "source errors: {maximum:?}"
        );
        assert!(
            agreement.iter().all(|&error| error <= 4),
            "BGRA agreement: {agreement:?}"
        );
    }
}

#[test]
fn nv12_non_idr_failure_recovers_and_mixed_outputs_work() {
    let codecs = VtCodecs::new();
    let size = PixelSize::new(256, 256);
    let Some(mut encoder) = available(codecs.encoder(size, 20_000_000, 30)) else {
        return;
    };
    let Some(mut decoder) = available(codecs.nv12_decoder()) else {
        return;
    };
    let (pixels, stride) = synthetic(size, 0);
    let mut idr = Vec::new();
    let mut packet = Vec::new();
    assert!(
        encoder
            .encode(&pixels, stride, size, false, &mut idr)
            .unwrap()
            .key
    );
    assert!(
        !encoder
            .encode(&pixels, stride, size, false, &mut packet)
            .unwrap()
            .key
    );
    let mut picture = Nv12::default();
    assert!(decoder.decode_nv12(&packet, &mut picture).is_err());
    assert_eq!(picture, Nv12::default());
    decoder.decode_nv12(&idr, &mut picture).unwrap();
    let y = picture.y.as_ptr();
    let uv = picture.uv.as_ptr();
    let mut bgra = Vec::new();
    decoder.decode(&packet, &mut bgra).unwrap();
    encoder
        .encode(&pixels, stride, size, false, &mut packet)
        .unwrap();
    decoder.decode_nv12(&packet, &mut picture).unwrap();
    assert_eq!(picture.y.as_ptr(), y, "reuse Y allocation");
    assert_eq!(picture.uv.as_ptr(), uv, "reuse UV allocation");
    encoder
        .encode(&pixels, stride, size, false, &mut packet)
        .unwrap();
    decoder.decode(&packet, &mut bgra).unwrap();
    // An IDR switches back to native BGRA; the following NV12 request must preserve
    // that session's references until another IDR permits rebuilding it.
    encoder
        .encode(&pixels, stride, size, true, &mut packet)
        .unwrap();
    decoder.decode(&packet, &mut bgra).unwrap();
    encoder
        .encode(&pixels, stride, size, false, &mut packet)
        .unwrap();
    decoder.decode_nv12(&packet, &mut picture).unwrap();
    picture.validate().unwrap();
}

#[repr(C)]
#[derive(Default)]
struct Timeval {
    seconds: std::ffi::c_long,
    microseconds: std::ffi::c_int,
}

#[repr(C)]
#[derive(Default)]
struct Rusage {
    user: Timeval,
    system: Timeval,
    counters: [std::ffi::c_long; 14],
}

unsafe extern "C" {
    fn getrusage(who: std::ffi::c_int, usage: *mut Rusage) -> std::ffi::c_int;
}

fn process_cpu_seconds() -> f64 {
    let mut usage = Rusage::default();
    // SAFETY: Darwin's public rusage layout, writable stack storage, RUSAGE_SELF=0.
    assert_eq!(unsafe { getrusage(0, &mut usage) }, 0);
    (usage.user.seconds + usage.system.seconds) as f64
        + f64::from(usage.user.microseconds + usage.system.microseconds) / 1_000_000.0
}

#[test]
#[ignore = "120-frame 1440p CPU comparison; run with --release --ignored --nocapture"]
fn input_pool_cpu_timing() {
    let size = PixelSize::new(2560, 1440);
    let mut measurements = Vec::new();
    for fallback in [true, false] {
        let Some(mut encoder) =
            available(VtCodecs::new().encoder_with_fallback(size, 20_000_000, 30, fallback))
        else {
            eprintln!(
                "{}: CPU time/frame unavailable (VideoToolbox session creation failed)",
                if fallback {
                    "fallback CVPixelBufferCreate"
                } else {
                    "IOSurface compression pool"
                }
            );
            continue;
        };
        let mut packet = Vec::new();
        let (mut pixels, stride) = synthetic(size, 0);
        encoder
            .encode(&pixels, stride, size, false, &mut packet)
            .unwrap();
        let start = process_cpu_seconds();
        let mut generation_cpu = 0.0;
        for frame in 0..120u32 {
            let generation_start = process_cpu_seconds();
            // Move the colour through every pixel without allocating per frame.
            for (index, pixel) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                pixel.copy_from_slice(&[
                    (index as u32 + frame) as u8,
                    (index as u32 / 2560 + frame * 3) as u8,
                    (index as u32 / 64 + frame * 5) as u8,
                    255,
                ]);
            }
            generation_cpu += process_cpu_seconds() - generation_start;
            encoder
                .encode(&pixels, stride, size, false, &mut packet)
                .unwrap();
        }
        let milliseconds = (process_cpu_seconds() - start - generation_cpu) * 1000.0 / 120.0;
        eprintln!(
            "{}: {milliseconds:.3} ms process CPU/frame (user + system, content generation excluded)",
            if fallback {
                "fallback CVPixelBufferCreate"
            } else {
                "IOSurface compression pool"
            }
        );
        measurements.push(milliseconds);
    }
    if measurements.len() != 2 {
        eprintln!(
            "Software-transfer inference unavailable; lead must run timing and sample on a Mac with VideoToolbox access."
        );
        return;
    }
    eprintln!(
        "pool/fallback CPU ratio {:.3}; {}. Lead must verify with sample.",
        measurements[1] / measurements[0],
        if measurements[1] < measurements[0] * 0.7 {
            "CPU reduction suggests software transfer avoided"
        } else {
            "no clear evidence that software transfer was avoided"
        }
    );
}
