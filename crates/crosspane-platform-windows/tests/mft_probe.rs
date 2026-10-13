//! Live probe: why does a machine with a hardware H.264 encoder end on the software one?
//!
//! Opens every hardware H.264 MFT one by one with the settings the agent uses and prints, for each,
//! the step that failed and its HRESULT, then does what the agent does (`MfCodecs::encoder_cpu`)
//! and prints the selection report. Synthetic buffers only: no window, capture or input.
//!
//! ```text
//! cargo test -p crosspane-platform-windows --features video --test mft_probe -- --ignored --nocapture
//! ```
#![cfg(all(windows, feature = "video"))]

use crosspane_media::codec::VideoEncoder;
use crosspane_platform_windows::video::{MfCodecs, SelectionReport, probe_hardware_encoders};
use crosspane_types::geom::PixelSize;

fn print_report(title: &str, report: &SelectionReport) {
    println!("  {title}:");
    for line in report.lines() {
        println!("    {line}");
    }
}

#[test]
#[ignore = "live probe of this machine's hardware H.264 MFTs; run with --ignored --nocapture"]
fn hardware_h264_encoders_report_why_they_are_refused() {
    for (size, bitrate, fps) in [
        (PixelSize::new(1920, 1080), 8_000_000, 60),
        (PixelSize::new(1280, 720), 4_000_000, 30),
    ] {
        println!(
            "== {}x{} @ {fps} fps, {bitrate} bit/s ==",
            size.width, size.height
        );
        let outcomes = probe_hardware_encoders(size, bitrate, fps).expect("probe");
        println!("  hardware encoders enumerated: {}", outcomes.len());
        for outcome in &outcomes {
            match (&outcome.refusal, outcome.first_frame_bytes) {
                (None, Some(bytes)) => println!(
                    "  OPENED  {}: first access unit {bytes} bytes",
                    outcome.candidate
                ),
                (Some(refusal), _) => println!("  REFUSED {refusal}"),
                (None, None) => println!("  ???     {}: no outcome", outcome.candidate),
            }
            for skipped in &outcome.skipped {
                println!("          {skipped}");
            }
        }

        // What the agent does.
        let codecs = MfCodecs::new();
        match codecs.encoder_cpu(size, bitrate, fps) {
            Ok(mut encoder) => {
                println!("  agent path encoder: {}", encoder.name());
                let pixels = vec![0x40_u8; size.width as usize * size.height as usize * 4];
                let mut out = Vec::new();
                let first = encoder.encode(&pixels, size.width * 4, size, false, &mut out);
                println!(
                    "  agent path first frame: {}",
                    first.map_or_else(
                        |error| error.to_string(),
                        |_| format!("{} bytes", out.len())
                    )
                );
                print_report(
                    "selection report after the first frame",
                    &encoder.selection_report(),
                );
                let _ = encoder.close();
            }
            Err(error) => {
                println!("  agent path encoder unavailable: {error}");
                if let Some(report) = codecs.last_selection_report() {
                    print_report("selection report", &report);
                }
            }
        }
        match codecs.decoder_cpu() {
            Ok(decoder) => {
                use crosspane_media::codec::VideoDecoder;
                println!("  agent path decoder: {}", decoder.name());
                print_report("decoder selection report", &decoder.selection_report());
                let _ = decoder.close();
            }
            Err(error) => println!("  agent path decoder unavailable: {error}"),
        }
    }
}
