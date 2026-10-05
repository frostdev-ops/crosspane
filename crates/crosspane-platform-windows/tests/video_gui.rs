//! Synthetic CPU buffers only. Native codecs run ONLY through Limited win-gui.
#![cfg(all(windows, feature = "video"))]
#![allow(unsafe_code, clippy::unwrap_used, clippy::expect_used)]

use crosspane_media::{
    codec::{VideoDecoder, VideoEncoder},
    picture::{Nv12, nv12_to_bgra},
};
use crosspane_platform_windows::{
    model::video::{Params, nals},
    video::{MfCodecs, MfDecoder, MfEncoder},
};
use crosspane_types::geom::PixelSize;
use std::{
    mem::size_of,
    ptr::null_mut,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{Foundation::CloseHandle, Security::*, System::Threading::*};

#[test]
fn codec_facades_are_send_and_factory_is_send_sync_without_construction() {
    fn send<T: Send>() {}
    fn both<T: Send + Sync>() {}
    send::<MfEncoder>();
    send::<MfDecoder>();
    both::<MfCodecs>();
}
fn limited() {
    let mut token = null_mut();
    let mut elevation = TOKEN_ELEVATION::default();
    let mut bytes = 0;
    // SAFETY: own process query token, exact initialized output, handle closed on every success path.
    unsafe {
        assert_ne!(
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token),
            0
        );
        let result = GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut bytes,
        );
        assert_ne!(CloseHandle(token), 0);
        assert_ne!(result, 0);
    }
    assert_eq!(
        elevation.TokenIsElevated, 0,
        "native codec probe requires Limited win-gui"
    );
}
struct Watchdog(mpsc::Sender<()>);
impl Watchdog {
    fn new() -> Self {
        let (send, receive) = mpsc::channel();
        thread::spawn(move || {
            if receive.recv_timeout(Duration::from_secs(40)).is_err() {
                eprintln!("OWN SYNTHETIC CODEC WATCHDOG EXPIRED");
                std::process::exit(124);
            }
        });
        Self(send)
    }
}
impl Drop for Watchdog {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}
fn pattern(size: PixelSize, frame: u32) -> (Vec<u8>, u32) {
    let stride = size.width * 4 + 12;
    let mut pixels = vec![0xee; stride as usize * size.height as usize];
    let patches = [
        [48, 80, 112, 255],
        [112, 80, 48, 255],
        [96, 144, 192, 255],
        [192, 144, 96, 255],
    ];
    for y in 0..size.height {
        for x in 0..size.width {
            let quadrant = u32::from(x >= size.width / 2) + 2 * u32::from(y >= size.height / 2);
            let mut pixel = patches[((quadrant + frame / 10) % 4) as usize];
            for channel in &mut pixel[..3] {
                *channel += (frame % 4 * 16) as u8;
            }
            let offset = (y * stride + x * 4) as usize;
            pixels[offset..offset + 4].copy_from_slice(&pixel);
        }
    }
    (pixels, stride)
}
fn psnr(source: &[u8], stride: u32, decoded: &[u8], real: PixelSize, coded: PixelSize) -> f64 {
    assert_eq!(
        decoded.len(),
        coded.width as usize * coded.height as usize * 4
    );
    let mut squared = 0.0;
    for y in 0..real.height {
        for x in 0..real.width {
            let a = (y * stride + x * 4) as usize;
            let b = (y * coded.width * 4 + x * 4) as usize;
            assert_eq!(decoded[b + 3], 255);
            for channel in 0..3 {
                squared += f64::from(source[a + channel].abs_diff(decoded[b + channel])).powi(2);
            }
        }
    }
    let mse = squared / (f64::from(real.width) * f64::from(real.height) * 3.0);
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0_f64.powi(2) / mse).log10()
    }
}
fn key(packet: &[u8]) {
    let types: Vec<_> = nals(packet).map(|nal| nal[0] & 31).collect();
    let idr = types
        .iter()
        .position(|kind| *kind == 5)
        .expect("requested IDR");
    assert!(types[..idr].contains(&7) && types[..idr].contains(&8));
}
#[test]
#[ignore = "Limited win-gui opt-in; synthetic buffers, no real window/capture/input"]
fn limited_synthetic_h264_round_trip_and_requested_idr() {
    assert_eq!(
        std::env::var("CROSSPANE_WINDOWS_VIDEO_PROBE").as_deref(),
        Ok("1")
    );
    limited();
    let _watchdog = Watchdog::new();
    let codecs = MfCodecs::new();
    let mut encoder = codecs
        .encoder_cpu(PixelSize::new(320, 240), 8_000_000, 30)
        .expect("encoder");
    let mut decoder = codecs.decoder_cpu().expect("decoder");
    let started = Instant::now();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut minimum = f64::INFINITY;
        let mut frames = 0;
        let mut picture = Nv12::default();
        for size in [
            PixelSize::new(320, 240),
            PixelSize::new(101, 75),
            PixelSize::new(336, 256),
        ] {
            let coded = Params::new(size, 1, 30).unwrap().coded().unwrap();
            for index in 0..10 {
                let (source, stride) = pattern(size, index);
                if index == 4 {
                    encoder.set_bitrate(1_000_000);
                }
                if index == 8 {
                    encoder.set_bitrate(8_000_000);
                }
                let force = index == 6;
                let mut packet = vec![0xee; 50_000];
                let encoded = encoder
                    .encode(&source, stride, size, force, &mut packet)
                    .unwrap_or_else(|error| panic!("encode frame {frames}: {error}"));
                if index == 0 || force {
                    assert!(encoded.key);
                }
                if encoded.key {
                    key(&packet);
                    let mut fresh = codecs.decoder_cpu().unwrap();
                    let mut decoded = Vec::new();
                    assert_eq!(fresh.decode(&packet, &mut decoded).unwrap(), coded);
                    assert!(psnr(&source, stride, &decoded, size, coded) >= 30.0);
                    fresh.close().expect("fresh decoder worker cleanup");
                }
                let allocations = (
                    picture.y.as_ptr(),
                    picture.y.capacity(),
                    picture.uv.as_ptr(),
                    picture.uv.capacity(),
                );
                decoder
                    .decode_nv12(&packet, &mut picture)
                    .unwrap_or_else(|error| panic!("decode frame {frames}: {error}"));
                assert_eq!(picture.size, coded);
                if allocations.1 >= picture.y.len() {
                    assert_eq!(picture.y.as_ptr(), allocations.0);
                }
                if allocations.3 >= picture.uv.len() {
                    assert_eq!(picture.uv.as_ptr(), allocations.2);
                }
                let mut decoded = Vec::new();
                nv12_to_bgra(&picture, coded, &mut decoded).unwrap();
                let quality = psnr(&source, stride, &decoded, size, coded);
                assert!(quality >= 30.0, "known-buffer PSNR {quality:.2}");
                minimum = minimum.min(quality);
                frames += 1;
            }
        }
        // Lost reference must fail; an explicit requested IDR must recover the same decoder.
        let size = PixelSize::new(336, 256);
        let (pixels, stride) = pattern(size, 0);
        let mut omitted = Vec::new();
        encoder
            .encode(&pixels, stride, size, false, &mut omitted)
            .unwrap();
        let mut dependent = Vec::new();
        let encoded = encoder
            .encode(&pixels, stride, size, false, &mut dependent)
            .unwrap();
        if !encoded.key {
            assert!(decoder.decode(&dependent, &mut Vec::new()).is_err());
        }
        let mut recovery = Vec::new();
        assert!(
            encoder
                .encode(&pixels, stride, size, true, &mut recovery)
                .unwrap()
                .key
        );
        assert_eq!(decoder.decode(&recovery, &mut Vec::new()).unwrap(), size);
        println!(
            "MFT encoder={} decoder={} frames={} minimum_PSNR={minimum:.2} elapsed_ms={} [P] VM only",
            encoder.name(),
            decoder.name(),
            frames,
            started.elapsed().as_millis()
        );
    }));
    let encoder_closed = encoder.close();
    let decoder_closed = decoder.close();
    println!(
        "responsive_worker_cleanup encoder={} decoder={}",
        encoder_closed.is_ok(),
        decoder_closed.is_ok()
    );
    encoder_closed.expect("encoder cleanup");
    decoder_closed.expect("decoder cleanup");
    result.unwrap();
}
