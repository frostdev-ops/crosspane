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

/// A single-slice stream exercises one decode thread even when slice threading is available.
#[test]
#[ignore = "120-frame 1440p CPU timing"]
fn nv12_decode_cpu_timing() {
    use crosspane_media::picture::Nv12;
    use ffmpeg_next::{self as ffmpeg, codec, format::Pixel, frame};
    use rustix::time::{ClockId, clock_gettime};

    fn cpu_seconds() -> f64 {
        let time = clock_gettime(ClockId::ThreadCPUTime);
        time.tv_sec as f64 + time.tv_nsec as f64 / 1_000_000_000.0
    }

    let name = "nv12_decode_cpu_timing";
    if !is_child(name) {
        let output = child_test(name, "software")
            .arg("--ignored")
            .output()
            .unwrap();
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        print!("{}", String::from_utf8_lossy(&output.stdout));
        assert!(output.status.success());
        return;
    }
    ffmpeg::init().unwrap();
    let size = PixelSize::new(2560, 1440);
    let codec = ffmpeg::encoder::find_by_name("libx264").unwrap();
    let mut context = codec::Context::new_with_codec(codec)
        .encoder()
        .video()
        .unwrap();
    context.set_width(size.width);
    context.set_height(size.height);
    context.set_format(Pixel::YUV420P);
    context.set_time_base((1, 30));
    context.set_bit_rate(20_000_000);
    context.set_gop(100_000);
    context.set_max_b_frames(0);
    context.set_threading(codec::threading::Config::count(1));
    context.set_colorspace(ffmpeg::color::Space::BT709);
    context.set_color_range(ffmpeg::color::Range::MPEG);
    let mut options = ffmpeg::Dictionary::new();
    options.set("preset", "ultrafast");
    options.set("tune", "zerolatency");
    options.set(
        "x264-params",
        "slices=1:annexb=1:repeat-headers=1:scenecut=0",
    );
    let mut encoder = context.open_as_with(codec, options).unwrap();
    let mut input = frame::Video::new(Pixel::YUV420P, size.width, size.height);
    input.data_mut(1).fill(96);
    input.data_mut(2).fill(160);
    let mut packets = Vec::new();
    for index in 0..120 {
        let stride = input.stride(0);
        for y in 0..size.height as usize {
            for x in 0..size.width as usize {
                input.data_mut(0)[y * stride + x] = (16 + (x / 16 + y / 16 + index) % 220) as u8;
            }
        }
        input.set_pts(Some(index as i64));
        encoder.send_frame(&input).unwrap();
        let mut packet = ffmpeg::Packet::empty();
        encoder.receive_packet(&mut packet).unwrap();
        let bytes = packet.data().unwrap();
        assert_eq!(
            nal_types(bytes)
                .iter()
                .filter(|kind| matches!(kind, 1 | 5))
                .count(),
            1
        );
        packets.push(bytes.to_vec());
    }
    let codecs = FfmpegCodecs::new().unwrap();
    let mut bgra_decoder = codecs.decoder_nv12().unwrap();
    let mut nv12_decoder = codecs.decoder_nv12().unwrap();
    let mut bgra = Vec::new();
    let mut nv12 = Nv12::default();
    // Warm both allocations and decoder contexts before measuring the same 120 access units.
    bgra_decoder.decode(&packets[0], &mut bgra).unwrap();
    nv12_decoder.decode_nv12(&packets[0], &mut nv12).unwrap();
    let start = cpu_seconds();
    for packet in &packets {
        assert_eq!(bgra_decoder.decode(packet, &mut bgra).unwrap(), size);
        std::hint::black_box(&bgra);
    }
    let old = (cpu_seconds() - start) * 1000.0 / 120.0;
    let start = cpu_seconds();
    for packet in &packets {
        nv12_decoder.decode_nv12(packet, &mut nv12).unwrap();
        assert_eq!(nv12.size, size);
        std::hint::black_box(&nv12);
    }
    let new = (cpu_seconds() - start) * 1000.0 / 120.0;
    eprintln!(
        "WP-2.23 single-thread CPU, 120 frames 2560x1440: BGRA {old:.3} ms/frame, NV12 {new:.3} ms/frame"
    );
}

#[cfg(feature = "cuda")]
mod cuda {
    use super::*;
    use crosspane_media::codec::NativeInputPool;
    use crosspane_platform_linux::video::{Nv12Layout, nv12_buffer};
    use std::sync::Arc;

    fn device(nvidia: bool) -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN))
            .into_iter()
            .find(|adapter| (adapter.get_info().vendor == 0x10de) == nvidia);
        let Some(adapter) = adapter else {
            eprintln!("SKIP CUDA: no matching Vulkan adapter (NVIDIA={nvidia})");
            return None;
        };
        eprintln!("WP-2.29 Vulkan adapter: {:?}", adapter.get_info());
        Some(
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap(),
        )
    }

    type NativeSetup = (
        Box<dyn VideoEncoder>,
        Arc<dyn NativeInputPool>,
        wgpu::Device,
        wgpu::Queue,
    );

    fn native(size: PixelSize) -> Option<NativeSetup> {
        // SAFETY: only check availability of the system driver, without any CUDA calls.
        if unsafe { libloading::Library::new("libcuda.so.1") }.is_err() {
            eprintln!("SKIP CUDA: libcuda.so.1 unavailable");
            return None;
        }
        let (device, queue) = device(true)?;
        let codecs = FfmpegCodecs::new().unwrap().with_gpu(device.clone());
        let mut encoder = encoder(&codecs, size);
        if encoder.name() != "h264_nvenc" {
            eprintln!("SKIP CUDA: NVENC unavailable");
            return None;
        }
        let pool = encoder
            .input_pool(size)
            .unwrap()
            .expect("NVIDIA CUDA setup must succeed (check driver/setup errors)");
        Some((encoder, pool, device, queue))
    }

    // Fill reusable host staging data; these test uploads replace WP-2.25's GPU writer.
    fn pattern(bgra: &mut [u8], size: PixelSize, index: u32) {
        for y in 0..size.height {
            for x in 0..size.width {
                let moving = ((x + index * 6) / 80 + (y + index * 4) / 80) % 2;
                let value = (40 + (x + y) * 130 / (size.width + size.height) + moving * 30) as u8;
                let offset = (y as usize * size.width as usize + x as usize) * 4;
                bgra[offset..offset + 4].copy_from_slice(&[value / 2 + 30, value, value + 30, 255]);
            }
        }
    }
    fn nv12(bgra: &[u8], size: PixelSize, layout: Nv12Layout, bytes: &mut [u8]) {
        let coded = padded(size);
        bytes.fill(0);
        let rgb = |x: u32, y: u32| {
            let offset =
                ((y.min(size.height - 1) * size.width + x.min(size.width - 1)) * 4) as usize;
            let b = f64::from(bgra[offset]);
            let g = f64::from(bgra[offset + 1]);
            let r = f64::from(bgra[offset + 2]);
            let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            [
                16.0 + luma * 219.0 / 255.0,
                128.0 + (b - luma) * 224.0 / (255.0 * 1.8556),
                128.0 + (r - luma) * 224.0 / (255.0 * 1.5748),
            ]
        };
        for y in 0..coded.height {
            for x in 0..coded.width {
                bytes[layout.y_offset as usize
                    + y as usize * layout.y_pitch as usize
                    + x as usize] = rgb(x, y)[0].round() as u8;
            }
        }
        for y in (0..coded.height).step_by(2) {
            for x in (0..coded.width).step_by(2) {
                let samples = [rgb(x, y), rgb(x + 1, y), rgb(x, y + 1), rgb(x + 1, y + 1)];
                let offset = layout.uv_offset as usize
                    + y as usize / 2 * layout.uv_pitch as usize
                    + x as usize;
                for channel in 1..3 {
                    bytes[offset + channel - 1] =
                        (samples.iter().map(|s| s[channel]).sum::<f64>() / 4.0).round() as u8;
                }
            }
        }
    }

    #[test]
    fn cuda_nv12_moving_pattern_sizes_and_idrs() {
        let first = PixelSize::new(1280, 720);
        let Some((mut native, mut pool, device, queue)) = native(first) else {
            return;
        };
        let codecs = FfmpegCodecs::new().unwrap();
        let mut cpu = encoder(&codecs, first);
        let mut native_decoder = codecs.decoder().unwrap();
        let mut cpu_decoder = codecs.decoder().unwrap();
        let mut colour_decoder = codecs.decoder_nv12().unwrap();
        let mut picture = crosspane_media::picture::Nv12::default();
        assert_eq!(
            native_decoder.name(),
            "h264",
            "run with the software decoder"
        );
        let mut native_packet = Vec::new();
        let mut cpu_packet = Vec::new();
        let mut native_pixels = Vec::new();
        let mut cpu_pixels = Vec::new();
        for size in [first, PixelSize::new(641, 359), PixelSize::new(960, 540)] {
            let next_pool = native.input_pool(size).unwrap().unwrap();
            if pool.size() != padded(size) {
                assert!(!Arc::ptr_eq(&pool, &next_pool));
            }
            pool = next_pool;
            assert_eq!(pool.size(), padded(size));
            let mut bgra = vec![0; size.width as usize * size.height as usize * 4];
            // The only native frame storage is the four persistent wgpu buffers. Frame metadata
            // and AVBuffer leases may allocate; native frames never call av_frame_get_buffer.
            let slots: Vec<_> = (0..4).map(|_| pool.acquire().unwrap()).collect();
            assert!(pool.acquire().is_err());
            let identities: Vec<_> = slots
                .iter()
                .map(|slot| nv12_buffer(slot.as_ref()).unwrap().0.clone())
                .collect();
            let (buffer, layout) = nv12_buffer(slots[0].as_ref()).unwrap();
            eprintln!("WP-2.29 size {size:?}, layout {layout:?}");
            for offset in [
                layout.y_offset,
                layout.uv_offset,
                u64::from(layout.y_pitch),
                u64::from(layout.uv_pitch),
            ] {
                assert_eq!(offset % 256, 0);
            }
            let mut staging = vec![0; buffer.size() as usize];
            drop(slots);
            let mut native_quality = 0.0;
            let mut cpu_quality = 0.0;
            for index in 0..120 {
                pattern(&mut bgra, size, index);
                nv12(&bgra, size, layout, &mut staging);
                let input = pool.acquire().unwrap();
                let (buffer, actual) = nv12_buffer(input.as_ref()).unwrap();
                assert_eq!(actual, layout);
                assert!(
                    identities.contains(buffer),
                    "no per-frame native frame storage allocation"
                );
                queue.write_buffer(buffer, 0, &staging);
                queue.submit([]);
                device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
                let forced = index == 37 || index == 91;
                let encoded = native
                    .encode_native(input.as_ref(), size, forced, &mut native_packet)
                    .unwrap();
                assert_eq!(encoded.key, index == 0 || forced);
                if encoded.key {
                    assert_idr(&native_packet);
                }
                if index == 0 {
                    colour_decoder
                        .decode_nv12(&native_packet, &mut picture)
                        .unwrap();
                    assert_eq!(
                        input.colour(),
                        crosspane_media::picture::YuvColour::default()
                    );
                    assert_eq!(picture.colour, input.colour());
                }
                let encoded_cpu = cpu
                    .encode(&bgra, size.width * 4, size, forced, &mut cpu_packet)
                    .unwrap();
                assert_eq!(encoded_cpu.key, index == 0 || forced);
                assert_eq!(
                    native_decoder
                        .decode(&native_packet, &mut native_pixels)
                        .unwrap(),
                    padded(size)
                );
                assert_eq!(
                    cpu_decoder.decode(&cpu_packet, &mut cpu_pixels).unwrap(),
                    padded(size)
                );
                native_quality += psnr(&bgra, size.width as usize * 4, &native_pixels, size);
                cpu_quality += psnr(&bgra, size.width as usize * 4, &cpu_pixels, size);
            }
            native_quality /= 120.0;
            cpu_quality /= 120.0;
            eprintln!(
                "WP-2.29 {size:?}: native PSNR {native_quality:.3} dB; CPU {cpu_quality:.3} dB"
            );
            // CPU packed RGB uses NVENC's BT.601 conversion, while native NV12 uses BT.709.
            // Their rounding differs: native can be more than 0.5 dB BETTER. Check the real
            // quality requirement (no regression beyond 0.5 dB), without rejecting improvement.
            assert!(native_quality + 0.5 >= cpu_quality);
            // Switching paths must rebuild and emit IDR, in either direction.
            assert!(
                native
                    .encode(&bgra, size.width * 4, size, false, &mut native_packet)
                    .unwrap()
                    .key
            );
            let input = pool.acquire().unwrap();
            let (buffer, _) = nv12_buffer(input.as_ref()).unwrap();
            queue.write_buffer(buffer, 0, &staging);
            queue.submit([]);
            device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            assert!(
                native
                    .encode_native(input.as_ref(), size, false, &mut native_packet)
                    .unwrap()
                    .key
            );
            native.set_bitrate(BITRATE / 2);
            assert!(
                native
                    .encode_native(input.as_ref(), size, false, &mut native_packet)
                    .unwrap()
                    .key
            );
            native.set_bitrate(BITRATE);
        }
    }

    #[test]
    fn cuda_non_nvidia_falls_back() {
        let Some((device, _)) = device(false) else {
            return;
        };
        let codecs = FfmpegCodecs::new().unwrap().with_gpu(device);
        let size = PixelSize::new(1280, 720);
        let mut encoder = encoder(&codecs, size);
        assert!(encoder.input_pool(size).unwrap().is_none());
        let pixels = vec![128; size.width as usize * size.height as usize * 4];
        assert!(
            encoder
                .encode(&pixels, size.width * 4, size, false, &mut Vec::new())
                .unwrap()
                .key
        );
    }

    // Linux getrusage ABI: two timeval fields and fourteen long counters.
    #[repr(C)]
    #[derive(Default)]
    struct Usage {
        user: [std::ffi::c_long; 2],
        system: [std::ffi::c_long; 2],
        counters: [std::ffi::c_long; 14],
    }
    unsafe extern "C" {
        fn getrusage(who: std::ffi::c_int, usage: *mut Usage) -> std::ffi::c_int;
    }
    fn cpu_seconds() -> f64 {
        let mut usage = Usage::default();
        // SAFETY: RUSAGE_THREAD (1) writes this Linux rusage layout to valid storage.
        assert_eq!(unsafe { getrusage(1, &mut usage) }, 0);
        (usage.user[0] + usage.system[0]) as f64
            + (usage.user[1] + usage.system[1]) as f64 / 1_000_000.0
    }
    #[test]
    fn cuda_cpu_time_1440p() {
        let size = PixelSize::new(2560, 1440);
        let Some((mut native, pool, device, queue)) = native(size) else {
            return;
        };
        let codecs = FfmpegCodecs::new().unwrap();
        let mut cpu = encoder(&codecs, size);
        let mut bgra = vec![0; size.width as usize * size.height as usize * 4];
        pattern(&mut bgra, size, 0);
        let input = pool.acquire().unwrap();
        let (buffer, layout) = nv12_buffer(input.as_ref()).unwrap();
        let mut staging = vec![0; buffer.size() as usize];
        nv12(&bgra, size, layout, &mut staging);
        queue.write_buffer(buffer, 0, &staging);
        queue.submit([]);
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let mut out = Vec::new();
        native
            .encode_native(input.as_ref(), size, false, &mut out)
            .unwrap();
        cpu.encode(&bgra, size.width * 4, size, false, &mut out)
            .unwrap();
        let start = cpu_seconds();
        for _ in 0..120 {
            native
                .encode_native(input.as_ref(), size, false, &mut out)
                .unwrap();
        }
        let native_ms = (cpu_seconds() - start) * 1000.0 / 120.0;
        let start = cpu_seconds();
        for _ in 0..120 {
            cpu.encode(&bgra, size.width * 4, size, false, &mut out)
                .unwrap();
        }
        let cpu_ms = (cpu_seconds() - start) * 1000.0 / 120.0;
        eprintln!(
            "WP-2.29 getrusage(RUSAGE_THREAD) 2560x1440: native {native_ms:.3} ms/frame, CPU BGRA {cpu_ms:.3} ms/frame (encode only, warmed sessions)"
        );
    }
}
