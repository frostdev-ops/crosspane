#![no_main]
use crosspane_media::audio::{AudioError, AudioPull, Encoder, JitterBuffer, wire::AudioPacket};
use crosspane_protocol::audio::{AudioKind, AudioStreamId};
use crosspane_types::time::MonoTime;
use libfuzzer_sys::fuzz_target;
use std::time::Duration;

fuzz_target!(|data: &[u8]| {
    let kind = if data.first().is_some_and(|v| v & 1 != 0) {
        AudioKind::Speaker
    } else {
        AudioKind::Microphone
    };
    let mut jitter = JitterBuffer::new(kind, AudioStreamId(1)).expect("codec");
    let mut encoder = Encoder::new(kind).expect("codec");
    let mut out = vec![0.0; kind.format().frame_samples()];
    let valid = encoder.encode(&out).expect("synthetic silence");
    let mut now = MonoTime::ZERO;
    for (index, chunk) in data.chunks(24).take(128).enumerate() {
        if chunk.len() < 16 {
            break;
        }
        let elapsed = if chunk[7] & 1 != 0 {
            // Include long pauses and saturated local time without unbounded catch-up.
            Duration::from_nanos(u64::from_le_bytes(chunk[8..16].try_into().expect("length")))
        } else {
            Duration::from_millis(u64::from(chunk[0]))
        };
        now = now.saturating_add(elapsed);
        let raw = u32::from_le_bytes(chunk[1..5].try_into().expect("length"));
        let seq = if chunk[5] & 1 != 0 {
            raw
        } else {
            index as u32 + u32::from(chunk[6] % 32)
        };
        let sample_time = if chunk[5] & 2 != 0 {
            u64::from_le_bytes(chunk[8..16].try_into().expect("length"))
        } else {
            u64::from(seq) * 480
        };
        let opus = if chunk[5] & 16 != 0 {
            vec![chunk[0]; 401]
        } else if chunk[5] & 4 != 0 {
            chunk[16..].to_vec()
        } else {
            valid.clone()
        };
        let _ = jitter.push(
            AudioPacket {
                stream: AudioStreamId(if chunk[5] & 8 != 0 { 0 } else { 1 }),
                seq,
                sample_time,
                opus,
            },
            now,
        );
        let result = jitter.pull(now, &mut out);
        // Arbitrary sender clocks may overflow during a long pause. This is a checked
        // InvalidPacket failure, with cleared output, rather than wrapped clock acceptance.
        assert!(matches!(result, Ok(_) | Err(AudioError::InvalidPacket)));
        if result.is_err() {
            assert!(out.iter().all(|v| *v == 0.0));
        }
        assert!(out.iter().all(|v| v.is_finite()));
        if result == Ok(AudioPull::Waiting) {
            assert!(out.iter().all(|v| *v == 0.0));
        }
        let stats = jitter.stats();
        assert!(stats.buffered_packets <= 32);
        assert!(stats.drift_ppm.abs() <= 1000);
        assert!(
            (Duration::from_millis(20)..=Duration::from_millis(80)).contains(&stats.target_delay)
        );
    }
});
