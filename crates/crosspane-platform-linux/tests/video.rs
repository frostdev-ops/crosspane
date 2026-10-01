#![cfg(feature = "ffmpeg")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    io::{Read, Write},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use crosspane_media::codec::{CodecError, VideoCodecs, VideoDecoder, VideoEncoder};
use crosspane_platform_linux::video::FfmpegCodecs;
use crosspane_types::geom::PixelSize;

const BITRATE: u32 = 8_000_000;
const FPS: u32 = 30;
const FRAMES: u32 = 30;

fn encoder(codecs: &FfmpegCodecs, size: PixelSize) -> Box<dyn VideoEncoder> {
    let encoder = codecs.encoder(size, BITRATE, FPS).unwrap();
    eprintln!("WP-2.14b backend: {}", encoder.name());
    // NVENC has a minimum frame size; smaller windows use libx264 on purpose.
    if std::env::var("CROSSPANE_REQUIRE_NVENC").as_deref() == Ok("1") && size.width >= 256 {
        assert_eq!(
            encoder.name(),
            "h264_nvenc",
            "NVENC is required for this run"
        );
    }
    encoder
}

fn selected_decoder(codecs: &FfmpegCodecs) -> Box<dyn VideoDecoder> {
    let decoder = codecs.decoder().unwrap();
    eprintln!("WP-2.14d decoder: {}", decoder.name());
    if std::env::var("CROSSPANE_REQUIRE_HW_DECODE").as_deref() == Ok("1") {
        assert!(
            matches!(decoder.name(), "h264 (cuda)" | "h264 (vaapi)"),
            "hardware decoding is required for this run: {}",
            decoder.name()
        );
    }
    decoder
}

// Test environment overrides belong to child processes: changing the parent environment is
// unsafe while FFmpeg and other tests have threads. Each child runs just the named test.
fn child_test(name: &str, backend: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", name, "--nocapture"])
        .env("CROSSPANE_VIDEO_TEST_CHILD", name)
        .env("CROSSPANE_VIDEO_DECODER", backend)
        .env_remove("CROSSPANE_REQUIRE_HW_DECODE");
    command
}

fn is_child(name: &str) -> bool {
    std::env::var("CROSSPANE_VIDEO_TEST_CHILD").as_deref() == Ok(name)
}

fn padded(size: PixelSize) -> PixelSize {
    PixelSize::new(size.width + size.width % 2, size.height + size.height % 2)
}

fn content(size: PixelSize, index: u32, stride: usize) -> Vec<u8> {
    // Row padding is deliberately nonzero so accidentally treating stride as width is visible.
    // The last row has only its pixel bytes: that is all the frozen buffer contract requires.
    let mut pixels = vec![0xcd; (size.height as usize - 1) * stride + size.width as usize * 4];
    let box_width = (size.width / 5).max(12) & !1;
    let box_height = (size.height / 4).max(12) & !1;
    let box_x = (index * 6 % (size.width - box_width)) & !1;
    let box_y = (index * 4 % (size.height - box_height)) & !1;
    for y in 0..size.height {
        for x in 0..size.width {
            let bgra = if (box_x..box_x + box_width).contains(&x)
                && (box_y..box_y + box_height).contains(&y)
            {
                [48, 144, 208, 255]
            } else {
                [
                    (32 + x * 160 / size.width) as u8,
                    (32 + y * 160 / size.height) as u8,
                    (48 + (x + y) * 128 / (size.width + size.height)) as u8,
                    255,
                ]
            };
            let offset = y as usize * stride + x as usize * 4;
            pixels[offset..offset + 4].copy_from_slice(&bgra);
        }
    }
    pixels
}

fn nal_types(data: &[u8]) -> Vec<u8> {
    assert!(data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1]));
    data.windows(4)
        .filter_map(|window| (window[..3] == [0, 0, 1]).then_some(window[3] & 0x1f))
        .collect()
}

fn assert_idr(data: &[u8]) {
    let types = nal_types(data);
    let sps = types.iter().position(|kind| *kind == 7).expect("SPS");
    let pps = types.iter().position(|kind| *kind == 8).expect("PPS");
    let idr = types.iter().position(|kind| *kind == 5).expect("IDR");
    assert!(
        sps < pps && pps < idr,
        "SPS/PPS must precede the IDR: {types:?}"
    );
}

fn psnr(source: &[u8], stride: usize, decoded: &[u8], size: PixelSize) -> f64 {
    let decoded_stride = padded(size).width as usize * 4;
    let mut squared_error = 0_u64;
    for y in 0..size.height as usize {
        for x in 0..size.width as usize {
            for component in 0..3 {
                let a = source[y * stride + x * 4 + component];
                let b = decoded[y * decoded_stride + x * 4 + component];
                squared_error += u64::from(a.abs_diff(b)).pow(2);
            }
            assert_eq!(decoded[y * decoded_stride + x * 4 + 3], 255);
        }
    }
    let mse = squared_error as f64 / (f64::from(size.width) * f64::from(size.height) * 3.0);
    10.0 * (255.0_f64.powi(2) / mse).log10()
}

// FNV-1a: enough to tell whether two decoders produced the same BGRA frame.
fn fingerprint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn round_trip(size: PixelSize) {
    let codecs = FfmpegCodecs::new().unwrap();
    let mut encoder = encoder(&codecs, size);
    let mut decoder = selected_decoder(&codecs);
    let stride = size.width as usize * 4 + 28;
    let mut encoded = vec![0xee; 100];
    let mut decoded = vec![0xee; 100];
    // These errors must not poison the already-open encoder session.
    for (pixels, stride, size) in [
        (&[][..], 0, PixelSize::new(0, 1)),
        (&[][..], size.width * 4 - 1, size),
        (&[][..], size.width * 4, size),
        (&[][..], u32::MAX, PixelSize::new(u32::MAX, 1)),
    ] {
        assert!(matches!(
            encoder.encode(pixels, stride, size, false, &mut encoded),
            Err(CodecError::BadInput(_))
        ));
        assert!(encoded.is_empty());
    }
    let mut encode_time = Duration::ZERO;
    let mut decode_time = Duration::ZERO;
    let mut bytes = 0;
    let mut min_psnr = f64::INFINITY;
    for index in 0..FRAMES {
        let source = content(size, index, stride);
        let force_key = index == 10 || index == 20;
        // Verify replacement on every call, including decoding a frame into an oversized vec.
        encoded.resize(50_000, 0xee);
        let start = Instant::now();
        let result = encoder
            .encode(&source, stride as u32, size, force_key, &mut encoded)
            .unwrap();
        encode_time += start.elapsed();
        bytes += encoded.len();
        assert_eq!(result.key, index == 0 || force_key, "frame {index}");
        if result.key {
            assert_idr(&encoded);
            // Each IDR must decode without any prior stream state.
            let mut independent = selected_decoder(&codecs);
            assert_eq!(
                independent.decode(&encoded, &mut Vec::new()).unwrap(),
                padded(size)
            );
        } else {
            assert!(!nal_types(&encoded).contains(&5));
        }
        decoded.resize(
            padded(size).width as usize * padded(size).height as usize * 4 + 128,
            0xee,
        );
        let start = Instant::now();
        let decoded_size = decoder.decode(&encoded, &mut decoded).unwrap();
        decode_time += start.elapsed();
        assert_eq!(decoded_size, padded(size));
        assert_eq!(
            decoded.len(),
            decoded_size.width as usize * decoded_size.height as usize * 4
        );
        let quality = psnr(&source, stride, &decoded, size);
        min_psnr = min_psnr.min(quality);
        assert!(
            quality >= 30.0,
            "{}×{} frame {index}: {quality:.2} dB",
            size.width,
            size.height
        );
        if size.height % 2 == 1 {
            let row = decoded_size.width as usize * 4;
            assert!(
                decoded[(size.height as usize - 1) * row..size.height as usize * row]
                    .iter()
                    .zip(&decoded[size.height as usize * row..])
                    .all(|(a, b)| a.abs_diff(*b) <= 8),
                "repeated padding can differ slightly after lossy conversion"
            );
        }
        if size.width % 2 == 1 {
            for row in decoded.chunks_exact(decoded_size.width as usize * 4) {
                let last = (size.width as usize - 1) * 4;
                // Lossy 4:2:0 can perturb neighbouring edge pixels slightly after conversion.
                assert!(
                    row[last..last + 4]
                        .iter()
                        .zip(&row[last + 4..last + 8])
                        .all(|(a, b)| a.abs_diff(*b) <= 8)
                );
            }
        }
    }
    eprintln!(
        "WP-2.14d encoder {}, decoder {} {}×{} at {BITRATE} bit/s: {FRAMES} frames, encode {:.3} ms/frame, decode {:.3} ms/frame, {:.1} bytes/frame, min PSNR {min_psnr:.2} dB",
        encoder.name(),
        decoder.name(),
        size.width,
        size.height,
        encode_time.as_secs_f64() * 1000.0 / f64::from(FRAMES),
        decode_time.as_secs_f64() * 1000.0 / f64::from(FRAMES),
        bytes as f64 / f64::from(FRAMES),
    );
}

#[test]
fn round_trip_320_240() {
    round_trip(PixelSize::new(320, 240));
}

#[test]
fn round_trip_1280_720() {
    round_trip(PixelSize::new(1280, 720));
}

#[test]
fn round_trip_3440_1440() {
    round_trip(PixelSize::new(3440, 1440));
}

#[test]
fn round_trip_odd_101_75() {
    round_trip(PixelSize::new(101, 75));
}

#[test]
fn size_change_is_an_idr() {
    let codecs = FfmpegCodecs::new().unwrap();
    let mut encoder = encoder(&codecs, PixelSize::new(320, 240));
    let mut decoder = selected_decoder(&codecs);
    let mut encoded = Vec::new();
    let mut decoded = Vec::new();
    for size in [
        PixelSize::new(320, 240),
        PixelSize::new(1280, 720),
        PixelSize::new(101, 75),
        // A source-size change also rebuilds when the padded dimensions remain the same.
        PixelSize::new(102, 76),
        PixelSize::new(320, 240),
    ] {
        for index in 0..2 {
            let stride = size.width as usize * 4;
            let source = content(size, index, stride);
            let result = encoder
                .encode(&source, stride as u32, size, false, &mut encoded)
                .unwrap();
            assert_eq!(result.key, index == 0);
            if index == 0 {
                assert_idr(&encoded);
            }
            assert_eq!(
                decoder.decode(&encoded, &mut decoded).unwrap(),
                padded(size)
            );
            assert!(psnr(&source, stride, &decoded, size) >= 30.0);
        }
    }
}

#[test]
fn non_idr_first_fails_then_idr_recovers() {
    let codecs = FfmpegCodecs::new().unwrap();
    let size = PixelSize::new(320, 240);
    let mut encoder = encoder(&codecs, size);
    let mut decoder = selected_decoder(&codecs);
    let mut encoded = Vec::new();
    let mut decoded = vec![0xef; 13];
    let stride = size.width as usize * 4;
    encoder
        .encode(
            &content(size, 0, stride),
            stride as u32,
            size,
            false,
            &mut encoded,
        )
        .unwrap();
    assert!(
        !encoder
            .encode(
                &content(size, 1, stride),
                stride as u32,
                size,
                false,
                &mut encoded
            )
            .unwrap()
            .key
    );
    assert!(matches!(
        decoder.decode(&encoded, &mut decoded),
        Err(CodecError::Failed(_))
    ));
    assert_eq!(decoded, vec![0xef; 13]);
    assert!(matches!(
        decoder.decode(&[], &mut decoded),
        Err(CodecError::Failed(_))
    ));
    assert!(
        encoder
            .encode(
                &content(size, 2, stride),
                stride as u32,
                size,
                true,
                &mut encoded
            )
            .unwrap()
            .key
    );
    assert_idr(&encoded);
    assert_eq!(decoder.decode(&encoded, &mut decoded).unwrap(), size);
    assert!(psnr(&content(size, 2, stride), stride, &decoded, size) >= 30.0);
    encoder
        .encode(
            &content(size, 3, stride),
            stride as u32,
            size,
            false,
            &mut encoded,
        )
        .unwrap();
    assert_eq!(decoder.decode(&encoded, &mut decoded).unwrap(), size);
    // A missing reference after synchronisation must also request an IDR.
    encoder
        .encode(
            &content(size, 4, stride),
            stride as u32,
            size,
            false,
            &mut encoded,
        )
        .unwrap();
    encoder
        .encode(
            &content(size, 5, stride),
            stride as u32,
            size,
            false,
            &mut encoded,
        )
        .unwrap();
    assert!(matches!(
        decoder.decode(&encoded, &mut decoded),
        Err(CodecError::Failed(_))
    ));
    encoder
        .encode(
            &content(size, 6, stride),
            stride as u32,
            size,
            true,
            &mut encoded,
        )
        .unwrap();
    assert_eq!(decoder.decode(&encoded, &mut decoded).unwrap(), size);
}

#[test]
fn bitrate_change_reduces_average_frame_size() {
    let codecs = FfmpegCodecs::new().unwrap();
    let size = PixelSize::new(1280, 720);
    let mut encoder = encoder(&codecs, size);
    let mut decoder = selected_decoder(&codecs);
    let stride = size.width as usize * 4;
    let mut encoded = Vec::new();
    let mut decoded = Vec::new();
    let mut averages = Vec::new();
    for bitrate in [BITRATE, 1_000_000] {
        encoder.set_bitrate(bitrate);
        let mut total = 0;
        for index in 0..FRAMES {
            let result = encoder
                .encode(
                    &content(size, index, stride),
                    stride as u32,
                    size,
                    false,
                    &mut encoded,
                )
                .unwrap();
            if index == 0 {
                assert!(result.key, "bitrate rebuild must apply on the next frame");
                assert_idr(&encoded);
            }
            total += encoded.len();
            assert_eq!(decoder.decode(&encoded, &mut decoded).unwrap(), size);
        }
        averages.push(total as f64 / f64::from(FRAMES));
    }
    eprintln!(
        "WP-2.14b {} bitrate change: 8 Mbit/s {:.1}, 1 Mbit/s {:.1} bytes/frame ({:.1}% reduction)",
        encoder.name(),
        averages[0],
        averages[1],
        100.0 * (1.0 - averages[1] / averages[0])
    );
    assert!(
        averages[1] <= averages[0] * 0.70,
        "frame size averages: {averages:?}"
    );
}

#[test]
fn forcing_software_gives_h264() {
    let name = "forcing_software_gives_h264";
    if is_child(name) {
        let codecs = FfmpegCodecs::new().unwrap();
        for _ in 0..2 {
            assert_eq!(codecs.decoder().unwrap().name(), "h264");
        }
        return;
    }
    let output = child_test(name, "software").output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn forcing_unavailable_backend_fails() {
    let name = "forcing_unavailable_backend_fails";
    if is_child(name) {
        let codecs = FfmpegCodecs::new().unwrap();
        for _ in 0..2 {
            assert!(matches!(codecs.decoder(), Err(CodecError::Unavailable(_))));
        }
        return;
    }
    // A deliberately nonexistent VA driver makes this deterministic even on a GPU-equipped
    // runner. No device options in product code are changed to accommodate this test.
    let output = child_test(name, "vaapi")
        .env("LIBVA_DRIVER_NAME", "crosspane_nonexistent_test_driver")
        .env("LIBVA_DRIVERS_PATH", "/crosspane_nonexistent_test_drivers")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn hardware_and_software_quality_agree() {
    let name = "hardware_and_software_quality_agree";
    let size = PixelSize::new(320, 240);
    let stride = size.width as usize * 4;
    let codecs = FfmpegCodecs::new().unwrap();
    if is_child(name) {
        let mut decoder = codecs.decoder().unwrap();
        assert_eq!(decoder.name(), "h264");
        let mut input = std::io::stdin().lock();
        let mut decoded = Vec::new();
        for index in 0..FRAMES {
            let mut length = [0; 4];
            input.read_exact(&mut length).unwrap();
            let mut encoded = vec![0; u32::from_le_bytes(length) as usize];
            input.read_exact(&mut encoded).unwrap();
            assert_eq!(decoder.decode(&encoded, &mut decoded).unwrap(), size);
            let quality = psnr(&content(size, index, stride), stride, &decoded, size);
            println!("SOFTWARE_PSNR {quality:.9} {:016x}", fingerprint(&decoded));
        }
        return;
    }
    let mut decoder = selected_decoder(&codecs);
    if decoder.name() == "h264" {
        eprintln!("hardware/software PSNR comparison skipped: hardware decoding unavailable");
        return;
    }
    let mut encoder = encoder(&codecs, size);
    let mut child = child_test(name, "software")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut encoded = Vec::new();
    let mut decoded = Vec::new();
    let mut hardware_quality = Vec::new();
    let mut hardware_fingerprints = Vec::new();
    for index in 0..FRAMES {
        let source = content(size, index, stride);
        encoder
            .encode(
                &source,
                stride as u32,
                size,
                index == 10 || index == 20,
                &mut encoded,
            )
            .unwrap();
        assert_eq!(decoder.decode(&encoded, &mut decoded).unwrap(), size);
        assert!(matches!(decoder.name(), "h264 (cuda)" | "h264 (vaapi)"));
        hardware_quality.push(psnr(&source, stride, &decoded, size));
        hardware_fingerprints.push(fingerprint(&decoded));
        // Give software the exact same access units, rather than encoding a second stream.
        input
            .write_all(&(encoded.len() as u32).to_le_bytes())
            .unwrap();
        input.write_all(&encoded).unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let (software_quality, software_fingerprints): (Vec<f64>, Vec<u64>) = stdout
        .lines()
        .filter_map(|line| line.strip_prefix("SOFTWARE_PSNR "))
        .map(|line| {
            let (quality, fingerprint) = line.split_once(' ').unwrap();
            (
                quality.parse::<f64>().unwrap(),
                u64::from_str_radix(fingerprint, 16).unwrap(),
            )
        })
        .unzip();
    assert_eq!(software_quality.len(), FRAMES as usize);
    // H.264 reconstruction is bit-exact and both decoders feed the same YUV420P→BGRA converter,
    // so the frames are normally identical. The contract is only the PSNR bound below (a
    // conforming driver may differ in rounding), but report how close they are.
    let identical = hardware_fingerprints
        .iter()
        .zip(&software_fingerprints)
        .filter(|(hardware, software)| hardware == software)
        .count();
    eprintln!(
        "WP-2.14d hardware vs software: {identical}/{FRAMES} frames bit-identical, max PSNR gap {:.3} dB",
        hardware_quality
            .iter()
            .zip(&software_quality)
            .map(|(hardware, software)| (hardware - software).abs())
            .fold(0.0, f64::max)
    );
    for (index, (hardware, software)) in hardware_quality.iter().zip(software_quality).enumerate() {
        assert!(
            (hardware - software).abs() <= 2.0,
            "frame {index}: hardware {hardware:.2} dB, software {software:.2} dB"
        );
    }
}
