#![allow(clippy::unwrap_used, clippy::expect_used)]
use crosspane_media::audio::{
    AudioError, AudioPull, Decoder, Encoder, JitterBuffer, wire::AudioPacket,
};
use crosspane_types::{
    audio::{AUDIO_FRAME_SAMPLES, AudioKind, AudioStreamId, MAX_OPUS_PACKET},
    time::MonoTime,
};
use proptest::prelude::*;
use std::time::Duration;

fn time(ms: u64) -> MonoTime {
    MonoTime::from_nanos(ms * 1_000_000)
}
fn tone(kind: AudioKind, seq: usize) -> Vec<f32> {
    (0..kind.format().frame_samples())
        .map(|i| {
            let channel = i % kind.format().channels as usize;
            let frame = seq * 480 + i / kind.format().channels as usize;
            let amplitude = if channel == 0 { 0.4 } else { 0.2 };
            amplitude * (std::f32::consts::TAU * 1000.0 * frame as f32 / 48000.0).sin()
        })
        .collect()
}
fn packet(encoder: &mut Encoder, kind: AudioKind, seq: u32, sample_time: u64) -> AudioPacket {
    AudioPacket {
        stream: AudioStreamId(1),
        seq,
        sample_time,
        opus: encoder.encode(&tone(kind, 0)).unwrap(),
    }
}
fn bounds(jitter: &JitterBuffer) {
    let stats = jitter.stats();
    assert!(stats.buffered_packets <= 32);
    assert!((Duration::from_millis(20)..=Duration::from_millis(80)).contains(&stats.target_delay));
    assert!(stats.drift_ppm.abs() <= 1000);
}

#[test]
fn synthetic_round_trips_and_silence() {
    eprintln!("linked codec: {}", opus::version());
    for kind in [AudioKind::Speaker, AudioKind::Microphone] {
        let mut encoder = Encoder::new(kind).unwrap();
        let mut decoder = Decoder::new(kind).unwrap();
        let channels = kind.format().channels as usize;
        let mut decoded = vec![0.0; kind.format().frame_samples()];
        let mut samples = vec![Vec::<f32>::new(); channels];
        for seq in 0..100 {
            let encoded = encoder.encode(&tone(kind, seq)).unwrap();
            assert!(!encoded.is_empty() && encoded.len() <= MAX_OPUS_PACKET);
            decoder.decode(Some(&encoded), false, &mut decoded).unwrap();
            assert!(decoded.iter().all(|v| v.is_finite()));
            if seq >= 20 {
                for (channel, signal) in samples.iter_mut().enumerate() {
                    signal.extend(decoded.iter().skip(channel).step_by(channels));
                }
            }
        }
        for (channel, signal) in samples.iter().enumerate() {
            let energy = |frequency: f64| {
                let (sin, cos) =
                    signal
                        .iter()
                        .enumerate()
                        .fold((0.0, 0.0), |(sin, cos), (i, v)| {
                            let angle = std::f64::consts::TAU * frequency * i as f64 / 48000.0;
                            (
                                sin + f64::from(*v) * angle.sin(),
                                cos + f64::from(*v) * angle.cos(),
                            )
                        });
                sin * sin + cos * cos
            };
            assert!(energy(1000.0) > 20.0 * energy(900.0));
            assert!(energy(1000.0) > 20.0 * energy(1100.0));
            let rms = (signal.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>()
                / signal.len() as f64)
                .sqrt();
            let expected = if channel == 0 { 0.4 } else { 0.2 } / 2.0f64.sqrt();
            assert!(
                (rms / expected - 1.0).abs() < 0.3,
                "{kind:?} channel {channel}: rms {rms}"
            );
        }
        let mut encoder = Encoder::new(kind).unwrap();
        let mut decoder = Decoder::new(kind).unwrap();
        let silence = vec![0.0; kind.format().frame_samples()];
        for _ in 0..8 {
            decoder
                .decode(
                    Some(&encoder.encode(&silence).unwrap()),
                    false,
                    &mut decoded,
                )
                .unwrap();
            assert!(decoded.iter().all(|v| v.is_finite() && v.abs() < 0.001));
        }
        decoder.decode(None, false, &mut decoded).unwrap();
        assert!(decoded.iter().all(|v| v.is_finite()));
    }
}

#[test]
fn malformed_inputs_clear_output() {
    for kind in [AudioKind::Speaker, AudioKind::Microphone] {
        let mut encoder = Encoder::new(kind).unwrap();
        let n = kind.format().frame_samples();
        assert_eq!(
            encoder.encode(&vec![0.0; n - 1]),
            Err(AudioError::InvalidFormat)
        );
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.01, 1.01] {
            let mut pcm = vec![0.0; n];
            pcm[7] = bad;
            assert_eq!(encoder.encode(&pcm), Err(AudioError::InvalidFormat));
        }
        let mut decoder = Decoder::new(kind).unwrap();
        for data in [vec![], vec![0; 401], vec![0xff], vec![0x03, 0xff]] {
            let mut out = vec![0.9; n];
            assert!(decoder.decode(Some(&data), false, &mut out).is_err());
            assert!(out.iter().all(|v| *v == 0.0));
        }
        // A well-formed 20 ms Opus packet is still invalid for this fixed 10 ms API.
        let mut raw = opus::Encoder::new(
            48000,
            if kind == AudioKind::Speaker {
                opus::Channels::Stereo
            } else {
                opus::Channels::Mono
            },
            opus::Application::Audio,
        )
        .unwrap();
        let longer = raw.encode_vec_float(&vec![0.0; n * 2], 400).unwrap();
        let mut out = vec![0.9; n];
        assert_eq!(
            decoder.decode(Some(&longer), false, &mut out),
            Err(AudioError::InvalidPacket)
        );
        assert!(out.iter().all(|v| *v == 0.0));
        let mut short = vec![0.9; n - 1];
        assert_eq!(
            decoder.decode(None, false, &mut short),
            Err(AudioError::InvalidFormat)
        );
        assert!(short.iter().all(|v| *v == 0.0));
    }
}

#[test]
fn reorder_duplicates_loss_corruption_wrap_and_window() {
    let kind = AudioKind::Speaker;
    let mut encoder = Encoder::new(kind).unwrap();
    let mut jitter = JitterBuffer::new(kind, AudioStreamId(1)).unwrap();
    assert!(JitterBuffer::new(kind, AudioStreamId(0)).is_err());
    let start = u32::MAX - 1;
    let first = packet(&mut encoder, kind, start, 1000);
    assert!(jitter.push(first.clone(), time(0)).unwrap());
    assert!(!jitter.push(first, time(1)).unwrap());
    for offset in [3, 1, 4] {
        assert!(
            jitter
                .push(
                    packet(
                        &mut encoder,
                        kind,
                        start.wrapping_add(offset),
                        1000 + u64::from(offset) * 480
                    ),
                    time(u64::from(offset) * 10)
                )
                .unwrap()
        );
    }
    let mut wrong = packet(&mut encoder, kind, 0, 1960);
    wrong.stream = AudioStreamId(2);
    assert_eq!(jitter.push(wrong, time(20)), Err(AudioError::WrongStream));
    assert_eq!(
        jitter.push(packet(&mut encoder, kind, 0, 1961), time(20)),
        Err(AudioError::InvalidPacket)
    );
    assert!(
        !jitter
            .push(
                packet(&mut encoder, kind, start.wrapping_add(32), 1000 + 32 * 480),
                time(20)
            )
            .unwrap()
    );
    let mut out = vec![0.0; 960];
    for ms in (40..=80).step_by(10) {
        jitter.pull(time(ms), &mut out).unwrap();
        assert!(out.iter().all(|v| v.is_finite()));
        bounds(&jitter);
    }
    assert!(jitter.stats().lost >= 1 && jitter.stats().concealed >= 1);
    assert!(
        !jitter
            .push(packet(&mut encoder, kind, start, 1000), time(90))
            .unwrap()
    );
    let seq = start.wrapping_add(6);
    let corrupt = AudioPacket {
        stream: AudioStreamId(1),
        seq,
        sample_time: 1000 + 6 * 480,
        opus: vec![0xff],
    };
    jitter.push(corrupt, time(90)).unwrap();
    for ms in (90..=500).step_by(10) {
        jitter.pull(time(ms), &mut out).unwrap();
        assert!(out.iter().all(|v| v.is_finite()));
    }
    assert!(out.iter().all(|v| *v == 0.0));
    assert!(jitter.pull(time(10_000), &mut out).is_ok());
    bounds(&jitter);
    let mut jitter = JitterBuffer::new(kind, AudioStreamId(1)).unwrap();
    for seq in 0..100 {
        let accepted = jitter
            .push(
                packet(&mut encoder, kind, seq, u64::from(seq) * 480),
                time(0),
            )
            .unwrap();
        assert_eq!(accepted, seq < 32);
        bounds(&jitter);
    }
    assert_eq!(jitter.stats().buffered_packets, 32);
    let mut overflow = JitterBuffer::new(kind, AudioStreamId(1)).unwrap();
    overflow
        .push(packet(&mut encoder, kind, 0, u64::MAX - 10), time(0))
        .unwrap();
    assert_eq!(
        overflow.push(packet(&mut encoder, kind, 1, 469), time(10)),
        Err(AudioError::InvalidPacket)
    );
}

// Exercise a ten-second pause, seq wrap, and pauses exceeding half/a whole seq cycle.
#[test]
fn long_pause_resumes_without_stale_audio_including_wrap() {
    for (start, slots) in [
        (100u32, 1000u64),
        (u32::MAX - 10, 1000),
        (17, u64::from(u32::MAX) + 100),
        (42, i32::MAX as u64 + 100),
    ] {
        let kind = AudioKind::Speaker;
        let mut encoder = Encoder::new(kind).unwrap();
        let mut jitter = JitterBuffer::new(kind, AudioStreamId(1)).unwrap();
        for offset in 0..8u32 {
            assert!(
                jitter
                    .push(
                        packet(
                            &mut encoder,
                            kind,
                            start.wrapping_add(offset),
                            1000 + u64::from(offset) * 480
                        ),
                        time(u64::from(offset) * 10)
                    )
                    .unwrap()
            );
        }
        let mut out = vec![0.0; 960];
        assert_eq!(jitter.pull(time(40), &mut out).unwrap(), AudioPull::Packet);
        assert!(out.iter().any(|v| v.abs() > 0.01));
        let resume = 50 + slots * 10;
        out.fill(0.9);
        assert_eq!(
            jitter.pull(time(resume), &mut out).unwrap(),
            AudioPull::Concealed
        );
        assert!(out.iter().all(|v| *v == 0.0));
        assert_eq!(jitter.stats().buffered_packets, 0);
        // First output consumed seq start; the paused slot and elapsed slots are discarded.
        let offset = slots + 2;
        let seq = start.wrapping_add(offset as u32);
        let stamp = 1000 + offset * 480;
        assert_eq!(
            jitter.push(packet(&mut encoder, kind, seq, stamp + 1), time(resume)),
            Err(AudioError::InvalidPacket)
        );
        let mut silent_encoder = Encoder::new(kind).unwrap();
        let silence = silent_encoder.encode(&vec![0.0; 960]).unwrap();
        for ahead in 0..8u32 {
            assert!(
                jitter
                    .push(
                        AudioPacket {
                            stream: AudioStreamId(1),
                            seq: seq.wrapping_add(ahead),
                            sample_time: stamp + u64::from(ahead) * 480,
                            opus: silence.clone()
                        },
                        time(resume)
                    )
                    .unwrap()
            );
        }
        assert_eq!(
            jitter.pull(time(resume + 1), &mut out).unwrap(),
            AudioPull::Waiting
        );
        assert!(out.iter().all(|v| *v == 0.0));
        assert_eq!(
            jitter
                .pull(
                    time(resume + jitter.stats().target_delay.as_millis() as u64),
                    &mut out
                )
                .unwrap(),
            AudioPull::Packet
        );
        assert!(out.iter().all(|v| v.is_finite() && v.abs() < 0.001));
        bounds(&jitter);
    }
    // A sender clock that cannot represent elapsed slots must fail without stale output.
    let kind = AudioKind::Speaker;
    let mut encoder = Encoder::new(kind).unwrap();
    let mut jitter = JitterBuffer::new(kind, AudioStreamId(1)).unwrap();
    jitter
        .push(packet(&mut encoder, kind, 0, u64::MAX - 480), time(0))
        .unwrap();
    let mut out = vec![0.9; 960];
    assert_eq!(
        jitter.pull(time(10_000), &mut out),
        Err(AudioError::InvalidPacket)
    );
    assert!(out.iter().all(|v| *v == 0.0));
    assert_eq!(jitter.stats().buffered_packets, 0);
}

// The sender clock runs independently during the pause; its sequence is not derived from
// receiver advancement. One hour at 100 ppm separates the clocks by 36 whole packets.
#[test]
fn one_hour_local_pause_recovers_independent_clocks_including_wrap() {
    for start in [100u32, u32::MAX - 180_000] {
        for ppm in [100i64, -100] {
            let kind = AudioKind::Speaker;
            let mut encoder = Encoder::new(kind).unwrap();
            let mut jitter = JitterBuffer::new(kind, AudioStreamId(1)).unwrap();
            for offset in 0..8u32 {
                jitter
                    .push(
                        packet(
                            &mut encoder,
                            kind,
                            start.wrapping_add(offset),
                            1000 + u64::from(offset) * 480,
                        ),
                        time(u64::from(offset) * 10),
                    )
                    .unwrap();
            }
            let mut out = vec![0.0; 960];
            jitter.pull(time(40), &mut out).unwrap();
            assert!(out.iter().any(|v| v.abs() > 0.01));
            let mut resume = 3_600_040u64;
            out.fill(0.9);
            assert_eq!(
                jitter.pull(time(resume), &mut out).unwrap(),
                AudioPull::Concealed
            );
            assert!(out.iter().all(|v| *v == 0.0));
            assert_eq!(jitter.stats().buffered_packets, 0);
            // Continued pulls before a packet arrives must not spend the exception, or
            // retain their interpolation silence across the eventual fresh boundary.
            resume += 10;
            assert_eq!(
                jitter.pull(time(resume), &mut out).unwrap(),
                AudioPull::Concealed
            );
            assert!(out.iter().all(|v| *v == 0.0));
            let sender_slots = (resume as i64 * (1_000_000 + ppm) / 10_000_000) as u64;
            let seq = start.wrapping_add(sender_slots as u32);
            let stamp = 1000 + sender_slots * 480;
            assert_eq!(
                jitter.push(packet(&mut encoder, kind, seq, stamp + 1), time(resume)),
                Err(AudioError::InvalidPacket)
            );
            // A corrupted forward packet cannot consume the recovery opportunity.
            assert!(
                jitter
                    .push(
                        AudioPacket {
                            stream: AudioStreamId(1),
                            seq,
                            sample_time: stamp,
                            opus: vec![0xff]
                        },
                        time(resume)
                    )
                    .is_err()
            );
            assert!(
                !jitter
                    .push(
                        packet(&mut encoder, kind, start.wrapping_add(7), 1000 + 7 * 480),
                        time(resume)
                    )
                    .unwrap()
            );
            let mut fresh_encoder = Encoder::new(kind).unwrap();
            let silence = fresh_encoder.encode(&vec![0.0; 960]).unwrap();
            for ahead in 0..8u32 {
                assert!(
                    jitter
                        .push(
                            AudioPacket {
                                stream: AudioStreamId(1),
                                seq: seq.wrapping_add(ahead),
                                sample_time: stamp + u64::from(ahead) * 480,
                                opus: silence.clone()
                            },
                            time(resume)
                        )
                        .unwrap()
                );
            }
            // Recovery is spent: an arbitrary active far-ahead jump cannot reanchor again.
            assert!(
                !jitter
                    .push(
                        packet(
                            &mut encoder,
                            kind,
                            seq.wrapping_add(1000),
                            stamp + 1000 * 480
                        ),
                        time(resume)
                    )
                    .unwrap()
            );
            assert_eq!(
                jitter.pull(time(resume + 1), &mut out).unwrap(),
                AudioPull::Waiting
            );
            assert!(out.iter().all(|v| *v == 0.0));
            let delay = jitter.stats().target_delay.as_millis() as u64;
            assert_eq!(
                jitter.pull(time(resume + delay), &mut out).unwrap(),
                AudioPull::Packet
            );
            assert!(out.iter().all(|v| v.is_finite() && v.abs() < 0.001));
            let mut audible = false;
            for tick in 1..20u32 {
                let ahead = tick + 7;
                assert!(
                    jitter
                        .push(
                            packet(
                                &mut fresh_encoder,
                                kind,
                                seq.wrapping_add(ahead),
                                stamp + u64::from(ahead) * 480
                            ),
                            time(resume + delay + u64::from(tick) * 10)
                        )
                        .unwrap()
                );
                jitter
                    .pull(time(resume + delay + u64::from(tick) * 10), &mut out)
                    .unwrap();
                assert!(out.iter().all(|v| v.is_finite()));
                audible |= out.iter().any(|v| v.abs() > 0.01);
                bounds(&jitter);
            }
            assert!(
                audible,
                "sender {ppm:+} ppm, start {start}: fresh audio must resume"
            );
        }
    }
}

#[test]
fn following_packet_fec_attempt_and_normal_reuse() {
    let kind = AudioKind::Microphone;
    let mut encoder = Encoder::new(kind).unwrap();
    let mut jitter = JitterBuffer::new(kind, AudioStreamId(1)).unwrap();
    let mut encoded = Vec::new();
    for seq in 0..8 {
        encoded.push(packet(&mut encoder, kind, seq, u64::from(seq) * 480));
    }
    assert_eq!(
        encoded[2].opus[0] & 0x80,
        0,
        "fixture must contain SILK/hybrid"
    );
    for seq in [0, 2, 3, 4, 5, 6, 7] {
        jitter
            .push(encoded[seq].clone(), time(seq as u64 * 10))
            .unwrap();
    }
    let mut out = vec![0.0; 480];
    let mut fec_pull = false;
    for ms in (40..=90).step_by(10) {
        fec_pull |= jitter.pull(time(ms), &mut out).unwrap() == AudioPull::Fec;
    }
    assert!(fec_pull);
    assert_eq!(jitter.stats().fec, 1);
    assert_eq!(jitter.stats().received, 7);
    assert_eq!(jitter.stats().lost, 1);
}

#[test]
fn startup_early_pulls_and_adaptive_jitter() {
    let kind = AudioKind::Speaker;
    let mut encoder = Encoder::new(kind).unwrap();
    let mut jitter = JitterBuffer::new(kind, AudioStreamId(1)).unwrap();
    let mut out = vec![0.9; 960];
    assert_eq!(jitter.pull(time(0), &mut out).unwrap(), AudioPull::Waiting);
    assert!(out.iter().all(|v| *v == 0.0));
    for seq in 0..5 {
        jitter
            .push(
                packet(&mut encoder, kind, seq, u64::from(seq) * 480),
                time(u64::from(seq) * 10),
            )
            .unwrap();
    }
    assert_eq!(jitter.stats().target_delay, Duration::from_millis(40));
    let before = jitter.stats();
    for ms in 0..40 {
        out.fill(0.9);
        assert_eq!(jitter.pull(time(ms), &mut out).unwrap(), AudioPull::Waiting);
        assert!(out.iter().all(|v| *v == 0.0));
    }
    assert_eq!(before, jitter.stats());
    assert_eq!(jitter.pull(time(40), &mut out).unwrap(), AudioPull::Packet);
    let before = jitter.stats();
    assert_eq!(jitter.pull(time(49), &mut out).unwrap(), AudioPull::Waiting);
    assert_eq!(before, jitter.stats());
    jitter
        .push(packet(&mut encoder, kind, 5, 2400), time(85))
        .unwrap();
    assert!(jitter.stats().target_delay > Duration::from_millis(40));
    bounds(&jitter);
}

#[test]
fn clocks_plus_and_minus_100_ppm_for_180_seconds() {
    for ppm in [100.0, -100.0] {
        let kind = AudioKind::Speaker;
        let mut encoder = Encoder::new(kind).unwrap();
        let encoded = encoder.encode(&vec![0.0; 960]).unwrap();
        let mut jitter = JitterBuffer::new(kind, AudioStreamId(1)).unwrap();
        let interval = 10_000_000.0 / (1.0 + ppm / 1_000_000.0);
        let mut seq = 0u32;
        let mut out = vec![0.0; 960];
        let mut means = [0.0; 3];
        let mut counts = [0; 3];
        let mut correction = 0.0;
        for tick in 0..18000u64 {
            let now = tick * 10_000_000;
            while (f64::from(seq) * interval).round() as u64 <= now {
                jitter
                    .push(
                        AudioPacket {
                            stream: AudioStreamId(1),
                            seq,
                            sample_time: u64::from(seq) * 480,
                            opus: encoded.clone(),
                        },
                        MonoTime::from_nanos((f64::from(seq) * interval).round() as u64),
                    )
                    .unwrap();
                seq += 1;
            }
            jitter.pull(MonoTime::from_nanos(now), &mut out).unwrap();
            assert_eq!(out.len(), 960);
            assert!(out.iter().all(|v| v.is_finite()));
            bounds(&jitter);
            if tick >= 6000 {
                let bucket = ((tick - 6000) / 4000) as usize;
                means[bucket] += jitter.stats().buffered_packets as f64;
                counts[bucket] += 1;
                if tick >= 14000 {
                    correction += f64::from(jitter.stats().drift_ppm);
                }
            }
        }
        for i in 0..3 {
            means[i] /= f64::from(counts[i]);
        }
        correction /= 4000.0;
        eprintln!(
            "clock {ppm:+} ppm: occupancy means {means:?}; final mean correction {correction}; stats {:?}",
            jitter.stats()
        );
        assert!(correction * ppm > 0.0, "correction direction");
        assert!(
            (means[2] - means[0]).abs() < 0.5,
            "occupancy trend {means:?}"
        );
        assert_eq!(jitter.stats().lost, 0);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, failure_persistence: None, ..ProptestConfig::default() })]
    #[test]
    fn arbitrary_schedules_preserve_bounds(schedule in prop::collection::vec((0u32..200, any::<u64>(), 0u16..500, prop::collection::vec(any::<u8>(), 0..420)), 0..150)) {
        let mut jitter = JitterBuffer::new(AudioKind::Microphone, AudioStreamId(1)).unwrap();
        let mut pcm = vec![f32::NAN; AUDIO_FRAME_SAMPLES];
        let mut now = 0u64;
        for (seq, sample_time, elapsed, opus) in schedule {
            now = now.saturating_add(u64::from(elapsed)*1_000_000);
            let sample_time = if sample_time & 1 == 0 { u64::from(seq)*480 } else { sample_time };
            let _ = jitter.push(AudioPacket { stream: AudioStreamId(1), seq, sample_time, opus }, MonoTime::from_nanos(now));
            jitter.pull(MonoTime::from_nanos(now), &mut pcm).unwrap();
            prop_assert!(pcm.iter().all(|v| v.is_finite()));
            bounds(&jitter);
        }
    }
}
