#![allow(clippy::unwrap_used)]
use crosspane_platform::{AudioDeviceError, AudioFormat, IoGate};

#[test]
fn open_reply_device_loss_is_failed_not_locked() {
    use crosspane_platform::PlatformError;
    use crosspane_platform_windows::model::audio::wait_open;
    let control = StreamControl::new(open_gate());
    let (send, reply) = std::sync::mpsc::sync_channel(1);
    send.send(Ok(())).unwrap();
    control.fail();
    assert!(matches!(
        wait_open(
            &reply,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
            &control
        ),
        Err(PlatformError::Backend(_))
    ));
}
#[test]
fn open_reply_cancelled_success_is_not_gate_evidence() {
    use crosspane_platform::PlatformError;
    use crosspane_platform_windows::model::audio::wait_open;
    let control = StreamControl::new(open_gate());
    let (send, reply) = std::sync::mpsc::sync_channel(1);
    send.send(Ok(())).unwrap();
    control.cancel();
    assert!(matches!(
        wait_open(
            &reply,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
            &control
        ),
        Err(PlatformError::Timeout)
    ));
}
#[test]
fn open_reply_gate_loss_is_locked() {
    use crosspane_platform::PlatformError;
    use crosspane_platform_windows::model::audio::wait_open;
    let gate = open_gate();
    let control = StreamControl::new(gate.clone());
    let (send, reply) = std::sync::mpsc::sync_channel(1);
    send.send(Ok(())).unwrap();
    gate.set_session_permits(false);
    assert!(matches!(
        wait_open(
            &reply,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
            &control
        ),
        Err(PlatformError::Locked)
    ));
}

mod allocations {
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
    };
    thread_local! {
        static TRACK: Cell<bool> = const { Cell::new(false) };
        static COUNT: Cell<usize> = const { Cell::new(0) };
    }
    pub struct Counting;
    fn record() {
        let _ = TRACK.try_with(|on| {
            if on.get() {
                let _ = COUNT.try_with(|n| n.set(n.get() + 1));
            }
        });
    }
    // SAFETY: this test allocator forwards every exact pointer/layout to the system allocator.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            record();
            // SAFETY: same valid layout forwarded without modifying the returned allocation.
            unsafe { System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            record();
            // SAFETY: same valid layout, preserving System's zero-initialization contract.
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            record();
            // SAFETY: exact live allocation/layout/size supplied by Rust's allocator contract.
            unsafe { System.realloc(pointer, layout, size) }
        }
        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            // SAFETY: exact original system allocation and layout.
            unsafe { System.dealloc(pointer, layout) }
        }
    }
    pub fn start() {
        COUNT.with(|n| n.set(0));
        TRACK.with(|v| v.set(true));
    }
    pub fn finish() -> usize {
        TRACK.with(|v| v.set(false));
        COUNT.with(Cell::get)
    }
}
#[global_allocator]
static ALLOCATOR: allocations::Counting = allocations::Counting;

#[test]
fn render_has_no_allocations_even_with_resampling_integer_conversion_and_underrun() {
    let (mut producer, mut consumer) = rtrb::RingBuffer::new(4096);
    for _ in 0..4096 {
        producer.push(0.001).unwrap();
    }
    let mut converter = Converter::new(
        STEREO,
        MixFormat::parse(&extensible(44_100, 2, 24, 1)).unwrap(),
    )
    .unwrap();
    let mut output = [0; 3072];
    allocations::start();
    let mut okay = true;
    for _ in 0..100 {
        okay &= converter.render(&mut consumer, 512, &mut output).is_ok();
    }
    let allocations = allocations::finish();
    assert!(okay);
    assert_eq!(allocations, 0);
}

#[test]
fn source_mono_duplicates_to_stereo_without_guessing_channel_positions() {
    let (mut producer, mut consumer) = rtrb::RingBuffer::new(4);
    producer.push(0.125).unwrap();
    let mut converter = Converter::new(
        AudioFormat {
            rate: 48_000,
            channels: 1,
        },
        MixFormat::parse(&wave(3, 48_000, 2, 32)).unwrap(),
    )
    .unwrap();
    let mut output = [0; 8];
    converter.render(&mut consumer, 1, &mut output).unwrap();
    assert_eq!(&output[..4], &0.125f32.to_le_bytes());
    assert_eq!(&output[4..], &0.125f32.to_le_bytes());
}

fn resampled(parts: &[usize]) -> Vec<u8> {
    let (mut producer, mut consumer) = rtrb::RingBuffer::new(8192);
    for i in 0..4096 {
        producer.push((i as f32 * 0.1).sin() * 0.01).unwrap();
        producer.push((i as f32 * 0.1).cos() * 0.01).unwrap();
    }
    let mut converter =
        Converter::new(STEREO, MixFormat::parse(&wave(3, 44_100, 2, 32)).unwrap()).unwrap();
    let mut result = Vec::new();
    for frames in parts {
        let mut output = vec![0; frames * 8];
        converter
            .render(&mut consumer, *frames, &mut output)
            .unwrap();
        result.extend(output);
    }
    result
}
#[test]
fn resampling_keeps_exact_phase_across_different_callback_sizes() {
    let whole = resampled(&[1000]);
    assert_eq!(whole, resampled(&[1, 31, 7, 256, 3, 702]));
    assert!(
        whole
            .as_chunks::<4>()
            .0
            .iter()
            .all(|b| f32::from_le_bytes(*b).is_finite())
    );
    assert!(whole.iter().any(|b| *b != 0));
}

#[test]
fn expired_open_refuses_even_an_already_queued_success() {
    use crosspane_platform::PlatformError;
    use crosspane_platform_windows::model::audio::wait_open;
    let control = StreamControl::new(open_gate());
    let (send, reply) = std::sync::mpsc::sync_channel(1);
    send.send(Ok(())).unwrap();
    let deadline = std::time::Instant::now() - std::time::Duration::from_millis(1);
    assert!(matches!(
        wait_open(&reply, deadline, &control),
        Err(PlatformError::Timeout)
    ));
    assert!(!control.permits());
}
#[test]
fn noncooperative_open_times_out_and_late_success_stays_disabled() {
    use crosspane_platform::PlatformError;
    use crosspane_platform_windows::model::audio::wait_open;
    use std::{
        sync::{Arc, mpsc},
        time::{Duration, Instant},
    };
    let control = Arc::new(StreamControl::new(open_gate()));
    let owner = control.clone();
    let (continue_send, continue_receive) = mpsc::sync_channel::<()>(1);
    let (send, reply) = mpsc::sync_channel(1);
    let thread = std::thread::spawn(move || {
        continue_receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(!owner.permits());
        assert!(!owner.is_retired());
        let _ = send.send(Ok(()));
        owner.retire();
    });
    let deadline = Instant::now() + Duration::from_millis(15);
    assert!(matches!(
        wait_open(&reply, deadline, &control),
        Err(PlatformError::Timeout)
    ));
    assert!(!control.is_retired());
    continue_send.send(()).unwrap();
    thread.join().unwrap();
    assert!(control.is_retired());
    assert!(!control.permits());
}

#[test]
fn noncooperative_retirement_returns_with_disabled_owner_still_retained() {
    use std::{
        sync::{Arc, mpsc},
        time::{Duration, Instant},
    };
    let control = Arc::new(StreamControl::new(open_gate()));
    let weak = Arc::downgrade(&control);
    let owner = control.clone();
    let (send, receive) = mpsc::sync_channel::<()>(1);
    let thread = std::thread::spawn(move || {
        receive.recv_timeout(Duration::from_secs(1)).unwrap();
        owner.retire();
    });
    control.cancel();
    control.wait_retired(Instant::now() + Duration::from_millis(5));
    assert!(!control.permits());
    assert!(!control.is_retired());
    drop(control);
    assert!(weak.upgrade().is_some());
    send.send(()).unwrap();
    thread.join().unwrap();
    assert!(weak.upgrade().is_none());
}

#[test]
fn format_boundaries_and_invalid_source_fail_before_render() {
    let mix = MixFormat::parse(&wave(3, 48_000, 2, 32)).unwrap();
    assert!(mix.bytes_for(16_385).is_err());
    assert!(
        Converter::new(
            AudioFormat {
                rate: 44_100,
                channels: 2
            },
            mix
        )
        .is_err()
    );
    assert!(
        Converter::new(
            AudioFormat {
                rate: 48_000,
                channels: 0
            },
            mix
        )
        .is_err()
    );
    let mut malformed = extensible(48_000, 2, 24, 1);
    malformed[18..20].copy_from_slice(&20u16.to_le_bytes());
    assert!(MixFormat::parse(&malformed).is_err());
}

#[cfg(windows)]
use crosspane_platform_windows::model;
#[cfg(windows)]
#[path = "../src/audio.rs"]
mod native;
use crosspane_platform_windows::model::audio::{Converter, MixFormat, StreamControl};

const STEREO: AudioFormat = AudioFormat {
    rate: 48_000,
    channels: 2,
};
fn wave(tag: u16, rate: u32, channels: u16, bits: u16) -> Vec<u8> {
    let align = channels * (bits / 8);
    let mut b = vec![0; 18];
    b[0..2].copy_from_slice(&tag.to_le_bytes());
    b[2..4].copy_from_slice(&channels.to_le_bytes());
    b[4..8].copy_from_slice(&rate.to_le_bytes());
    b[8..12].copy_from_slice(&(rate * u32::from(align)).to_le_bytes());
    b[12..14].copy_from_slice(&align.to_le_bytes());
    b[14..16].copy_from_slice(&bits.to_le_bytes());
    b
}
fn extensible(rate: u32, channels: u16, bits: u16, subtype: u32) -> Vec<u8> {
    let mut b = wave(0xfffe, rate, channels, bits);
    b[16..18].copy_from_slice(&22u16.to_le_bytes());
    b.extend_from_slice(&bits.to_le_bytes());
    b.extend_from_slice(&(if channels == 2 { 3u32 } else { 4u32 }).to_le_bytes());
    b.extend_from_slice(&subtype.to_le_bytes());
    b.extend_from_slice(&[0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71]);
    b
}
fn convert(format: &[u8], samples: &[f32], frames: usize) -> Vec<u8> {
    let mix = MixFormat::parse(format).unwrap();
    let width = usize::from(u16::from_le_bytes([format[12], format[13]]));
    let mut converter = Converter::new(STEREO, mix).unwrap();
    let (mut producer, mut consumer) = rtrb::RingBuffer::new(samples.len().max(2));
    for sample in samples {
        producer.push(*sample).unwrap();
    }
    let mut output = vec![0xa5; frames * width];
    converter
        .render(&mut consumer, frames, &mut output)
        .unwrap();
    output
}
#[test]
fn admitted_extensible_float_uses_exact_rate_and_channels() {
    let mix = MixFormat::parse(&extensible(44_100, 2, 32, 3)).unwrap();
    assert_eq!((mix.rate(), mix.channels()), (44_100, 2));
}
#[test]
fn malformed_mix_fields_and_unknown_layout_refuse() {
    let valid = extensible(48_000, 2, 32, 3);
    for offset in [0, 2, 8, 12, 14, 16, 18, 20, 24, 39] {
        let mut malformed = valid.clone();
        malformed[offset] ^= 0x80;
        assert!(MixFormat::parse(&malformed).is_err(), "field at {offset}");
    }
    let mut zero_rate = valid.clone();
    zero_rate[4..8].fill(0);
    assert!(MixFormat::parse(&zero_rate).is_err());
    for len in 0..valid.len() {
        assert!(MixFormat::parse(&valid[..len]).is_err());
    }
    let mut extra = valid;
    extra.push(0);
    assert!(MixFormat::parse(&extra).is_err());
    assert!(MixFormat::parse(&wave(3, 48_000, 6, 32)).is_err());
}
#[test]
fn float_identity_preserves_pairs_and_sanitizes_nonfinite() {
    let output = convert(
        &wave(3, 48_000, 2, 32),
        &[0.25, -0.5, f32::NAN, f32::INFINITY],
        2,
    );
    let got: Vec<_> = output
        .as_chunks::<4>()
        .0
        .iter()
        .map(|v| f32::from_le_bytes(*v))
        .collect();
    assert_eq!(got, [0.25, -0.5, 0.0, 0.0]);
}
#[test]
fn integer_pcm_clips_and_uses_exact_container_width() {
    for (bits, expected) in [
        (16, vec![255, 127, 0, 128]),
        (24, vec![255, 255, 127, 0, 0, 128]),
        (32, vec![255, 255, 255, 127, 0, 0, 0, 128]),
    ] {
        assert_eq!(
            convert(&wave(1, 48_000, 2, bits), &[2.0, -2.0], 1),
            expected
        );
    }
}
#[test]
fn stereo_to_mono_is_finite_average_and_underrun_is_silence() {
    let output = convert(&wave(3, 48_000, 1, 32), &[0.25, 0.75], 2);
    let got: Vec<_> = output
        .as_chunks::<4>()
        .0
        .iter()
        .map(|v| f32::from_le_bytes(*v))
        .collect();
    assert_eq!(got, [0.5, 0.0]);
}
#[test]
fn incomplete_input_frame_waits_without_rotating_channels() {
    let (mut producer, mut consumer) = rtrb::RingBuffer::new(4);
    let mut converter =
        Converter::new(STEREO, MixFormat::parse(&wave(3, 48_000, 2, 32)).unwrap()).unwrap();
    producer.push(0.25).unwrap();
    let mut output = [0xa5; 8];
    converter.render(&mut consumer, 1, &mut output).unwrap();
    assert_eq!(output, [0; 8]);
    assert_eq!(consumer.slots(), 1);
    producer.push(-0.5).unwrap();
    converter.render(&mut consumer, 1, &mut output).unwrap();
    assert_eq!(&output[..4], &0.25f32.to_le_bytes());
    assert_eq!(&output[4..], &(-0.5f32).to_le_bytes());
}
#[test]
fn conversion_rejects_wrong_output_extent_before_consumption() {
    let (mut producer, mut consumer) = rtrb::RingBuffer::new(4);
    producer.push(0.25).unwrap();
    producer.push(-0.5).unwrap();
    let mut converter =
        Converter::new(STEREO, MixFormat::parse(&wave(3, 48_000, 2, 32)).unwrap()).unwrap();
    assert!(converter.render(&mut consumer, 1, &mut [0; 7]).is_err());
    assert_eq!(consumer.slots(), 2);
}
fn open_gate() -> std::sync::Arc<IoGate> {
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    gate
}
#[test]
fn gate_close_reopen_between_ticks_latches_until_deliberate_new_stream() {
    let gate = open_gate();
    let control = StreamControl::new(gate.clone());
    assert!(control.permits());
    gate.set_session_permits(false);
    gate.set_session_permits(true);
    assert!(!control.permits());
    assert_eq!(control.reason(), Some(AudioDeviceError::Locked));
    assert!(!control.permits());
    assert!(StreamControl::new(gate).permits());
}
#[test]
fn cancel_and_device_failure_are_terminal_with_distinct_evidence() {
    let control = StreamControl::new(open_gate());
    assert!(control.permits());
    control.cancel();
    assert!(!control.permits());
    assert_eq!(control.reason(), None);
    let failed = StreamControl::new(open_gate());
    failed.fail();
    assert!(!failed.permits());
    assert_eq!(failed.reason(), Some(AudioDeviceError::Failed));
    failed.cancel();
    assert_eq!(failed.reason(), Some(AudioDeviceError::Failed));
}
