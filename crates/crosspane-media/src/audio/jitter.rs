use super::{AudioError, AudioPull, AudioStats, Decoder, validate_packet, wire::AudioPacket};
use crosspane_types::{
    audio::{AUDIO_FRAME_SAMPLES, AudioKind, AudioStreamId, MAX_OPUS_PACKET},
    time::MonoTime,
};
use std::{fmt, time::Duration};

const SLOT: Duration = Duration::from_millis(10);
const WINDOW: u32 = 32;
const RING_FRAMES: usize = 3 * AUDIO_FRAME_SAMPLES;
const STARVATION: u32 = 20; // At most 200 ms of PLC; then reset once and emit silence.

/// Arrival jitter uses a 1/16 EWMA of the absolute local interarrival error against sequence
/// distance (never sender wall time). Target is 40 ms + four jitter deviations, plus a loss
/// margin decaying by 1/256 per received packet, clamped to 20..80 ms. Occupancy uses a 1/64
/// EWMA, a one-packet deadband, and a slow PI servo (20 ppm/frame proportional, 0.1 ppm/frame/slot integral).
/// Positive correction consumes more source samples. The fixed three-frame interpolation ring
/// retains fractional position and channel continuity across packets. Late pulls advance all
/// elapsed source slots with checked constant arithmetic, discard queued/interpolated audio,
/// and return silence while rebasing the deadline. A local pause of at least eight slots arms
/// one fresh reanchor: the next decodable forward packet must match the preserved cumulative
/// sender clock, including any whole sequence wraps. This tolerates sender drift beyond the
/// normal window without letting active-playout jumps reset admission. The first such packet
/// starts a new target-delay wait; delayed forward packets cannot be distinguished from fresh
/// packets without a wire freshness signal. No catch-up decoding or stored latency.
/// All queues and scratch are allocated by `new`.
pub struct JitterBuffer {
    kind: AudioKind,
    stream: AudioStreamId,
    decoder: Decoder,
    packets: Vec<AudioPacket>,
    anchor: Option<(u32, u64)>,
    recovery_anchor: Option<(u32, u64)>,
    seq: u32,
    due: Option<MonoTime>,
    arrival: Option<(u32, MonoTime)>,
    jitter_ns: f64,
    rate_anchor: Option<(u32, MonoTime)>,
    rate_ppm: f64,
    loss_ns: f64,
    stats: AudioStats,
    scratch: Vec<f32>,
    ring: Vec<f32>,
    head: usize,
    len: usize,
    phase: f64,
    occupancy: Option<f64>,
    integral: f64,
    starving: u32,
}

impl fmt::Debug for JitterBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JitterBuffer")
            .field("kind", &self.kind)
            .field("stream", &self.stream)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl JitterBuffer {
    pub fn new(kind: AudioKind, stream: AudioStreamId) -> Result<Self, AudioError> {
        if stream.0 == 0 {
            return Err(AudioError::WrongStream);
        }
        Ok(Self {
            kind,
            stream,
            decoder: Decoder::new(kind)?,
            packets: Vec::with_capacity(WINDOW as usize),
            anchor: None,
            recovery_anchor: None,
            seq: 0,
            due: None,
            arrival: None,
            jitter_ns: 0.0,
            rate_anchor: None,
            rate_ppm: 0.0,
            loss_ns: 0.0,
            stats: AudioStats {
                target_delay: Duration::from_millis(40),
                buffered_packets: 0,
                received: 0,
                lost: 0,
                concealed: 0,
                fec: 0,
                dropped: 0,
                drift_ppm: 0,
            },
            scratch: vec![0.0; kind.format().frame_samples()],
            ring: vec![0.0; RING_FRAMES * kind.format().channels as usize],
            head: 0,
            len: 0,
            phase: 0.0,
            occupancy: None,
            integral: 0.0,
            starving: 0,
        })
    }

    pub fn push(&mut self, packet: AudioPacket, now: MonoTime) -> Result<bool, AudioError> {
        if packet.stream.0 == 0 || packet.stream != self.stream {
            return Err(AudioError::WrongStream);
        }
        if packet.opus.is_empty() || packet.opus.len() > MAX_OPUS_PACKET {
            return Err(AudioError::InvalidPacket);
        }
        if let Some((seq, sample_time)) = self.recovery_anchor {
            // Sender sample distance disambiguates whole u32 wraps. Never use local time
            // as a sender clock, or grant this exception outside a locally observed pause.
            let Some(samples) = packet.sample_time.checked_sub(sample_time) else {
                self.stats.dropped = self.stats.dropped.saturating_add(1);
                return Ok(false);
            };
            if samples % AUDIO_FRAME_SAMPLES as u64 != 0
                || seq.wrapping_add((samples / AUDIO_FRAME_SAMPLES as u64) as u32) != packet.seq
            {
                return Err(AudioError::InvalidPacket);
            }
            if samples == 0 {
                self.stats.dropped = self.stats.dropped.saturating_add(1);
                return Ok(false);
            }
            // Invalid codec data cannot spend the one-shot exception. Probe with the already
            // reset decoder and preallocated scratch, then reset again before real playout.
            let decoded = self
                .decoder
                .decode(Some(&packet.opus), false, &mut self.scratch);
            self.scratch.fill(0.0);
            self.decoder.reset()?;
            decoded?;
            // Pulls while waiting for recovery may have buffered silence and advanced the
            // servo. Restart interpolation at the fresh source boundary as well.
            self.packets.clear();
            self.ring.fill(0.0);
            self.head = 0;
            self.len = 0;
            self.phase = 0.0;
            self.occupancy = None;
            self.integral = 0.0;
            self.arrival = None;
            self.rate_anchor = None;
            self.rate_ppm = 0.0;
            self.stats.drift_ppm = 0;
            self.starving = STARVATION + 1;
            self.seq = packet.seq;
            self.anchor = Some((packet.seq, packet.sample_time));
            self.due = Some(now.saturating_add(self.stats.target_delay));
            self.recovery_anchor = None;
        }
        if let Some((anchor_seq, anchor_time)) = self.anchor {
            let offset =
                i64::from(packet.seq.wrapping_sub(anchor_seq) as i32) * AUDIO_FRAME_SAMPLES as i64;
            if anchor_time.checked_add_signed(offset) != Some(packet.sample_time) {
                return Err(AudioError::InvalidPacket);
            }
            if packet.seq.wrapping_sub(self.seq) >= WINDOW
                || self.packets.iter().any(|p| p.seq == packet.seq)
                || self.packets.len() >= WINDOW as usize
            {
                self.stats.dropped = self.stats.dropped.saturating_add(1);
                return Ok(false);
            }
        } else {
            self.seq = packet.seq;
            self.due = Some(now.saturating_add(self.stats.target_delay));
        }
        if self
            .anchor
            .is_none_or(|(seq, _)| (packet.seq.wrapping_sub(seq) as i32) > 0)
        {
            self.anchor = Some((packet.seq, packet.sample_time));
        }
        if let Some((seq, time)) = self.arrival {
            let distance = packet.seq.wrapping_sub(seq);
            if distance > 0 && distance < WINDOW && now >= time {
                let elapsed = now.saturating_duration_since(time).as_nanos() as f64;
                let deviation = (elapsed - f64::from(distance) * SLOT.as_nanos() as f64).abs();
                self.jitter_ns += (deviation - self.jitter_ns) / 16.0;
            }
        }
        if self
            .arrival
            .is_none_or(|(seq, _)| (packet.seq.wrapping_sub(seq) as i32) > 0)
        {
            self.arrival = Some((packet.seq, now));
        }
        // Estimate clock rate over >= one second, using only newer sequence arrivals.
        // The endpoint estimate is clamped and smoothed by 1/8; packet jitter cannot command
        // an unbounded correction. This feed-forward term observes sub-packet clock drift.
        match self.rate_anchor {
            None => self.rate_anchor = Some((packet.seq, now)),
            Some((seq, time)) => {
                let distance = packet.seq.wrapping_sub(seq);
                if (100..=i32::MAX as u32).contains(&distance) && now > time {
                    let elapsed = now.saturating_duration_since(time).as_nanos() as f64;
                    let estimate = (f64::from(distance) * SLOT.as_nanos() as f64 / elapsed - 1.0)
                        * 1_000_000.0;
                    self.rate_ppm += (estimate.clamp(-1000.0, 1000.0) - self.rate_ppm) / 8.0;
                    self.rate_anchor = Some((packet.seq, now));
                }
            }
        }
        self.loss_ns *= 255.0 / 256.0;
        self.target();
        self.stats.received = self.stats.received.saturating_add(1);
        self.packets.push(packet);
        Ok(true)
    }

    fn target(&mut self) {
        let ns =
            (40_000_000.0 + 4.0 * self.jitter_ns + self.loss_ns).clamp(20_000_000.0, 80_000_000.0);
        self.stats.target_delay = Duration::from_nanos(ns as u64);
    }

    pub fn stats(&self) -> AudioStats {
        AudioStats {
            buffered_packets: self.packets.len(),
            ..self.stats
        }
    }

    pub fn pull(&mut self, now: MonoTime, pcm: &mut [f32]) -> Result<AudioPull, AudioError> {
        pcm.fill(0.0);
        if pcm.len() != self.kind.format().frame_samples() {
            return Err(AudioError::InvalidFormat);
        }
        let Some(due) = self.due else {
            return Ok(AudioPull::Waiting);
        };
        if now < due {
            return Ok(AudioPull::Waiting);
        }
        let late = now.saturating_duration_since(due).as_nanos() / SLOT.as_nanos();
        if late >= 8 {
            // seq points beyond the decoded ring. Rewind its remaining source frames before
            // advancing elapsed slots, including the current slot that we silence below.
            let current = self
                .seq
                .wrapping_sub(self.len.div_ceil(AUDIO_FRAME_SAMPLES) as u32);
            let skip = u64::try_from(late).map_err(|_| AudioError::InvalidPacket)?;
            let advance = skip.checked_add(1).ok_or(AudioError::InvalidPacket)?;
            let (anchor_seq, anchor_time) = self.anchor.ok_or(AudioError::InvalidPacket)?;
            // Preserve the actual sender anchor before replacing it with nominal advancement.
            // Repeated late pulls must not overwrite an outstanding recovery opportunity.
            if self.recovery_anchor.is_none() {
                self.recovery_anchor = Some((anchor_seq, anchor_time));
            }
            let offset =
                i64::from(current.wrapping_sub(anchor_seq) as i32) * AUDIO_FRAME_SAMPLES as i64;
            let sample_time = anchor_time.checked_add_signed(offset).and_then(|time| {
                advance
                    .checked_mul(AUDIO_FRAME_SAMPLES as u64)
                    .and_then(|samples| time.checked_add(samples))
            });
            // Clear stale audio even if the sender sample clock cannot represent the advance.
            self.stats.dropped = self.stats.dropped.saturating_add(self.packets.len() as u64);
            self.packets.clear();
            self.decoder.reset()?;
            self.scratch.fill(0.0);
            self.ring.fill(0.0);
            self.head = 0;
            self.len = 0;
            self.phase = 0.0;
            self.occupancy = None;
            self.integral = 0.0;
            self.arrival = None;
            self.rate_anchor = None;
            self.rate_ppm = 0.0;
            self.stats.drift_ppm = 0;
            self.starving = STARVATION + 1;
            let sample_time = sample_time.ok_or(AudioError::InvalidPacket)?;
            self.seq = current.wrapping_add(advance as u32);
            // Rebase the sender anchor too: advances may exceed half or multiple seq wraps.
            // Future packets still have to match the checked, cumulative sample clock.
            self.anchor = Some((self.seq, sample_time));
            self.stats.lost = self.stats.lost.saturating_add(advance);
            self.stats.concealed = self.stats.concealed.saturating_add(1);
            self.due = Some(now.saturating_add(SLOT));
            return Ok(AudioPull::Concealed);
        }
        self.due = Some(due.saturating_add(SLOT));
        let occupancy =
            self.packets.len() as f64 + (self.len as f64 - self.phase) / AUDIO_FRAME_SAMPLES as f64;
        let smoothed = self.occupancy.unwrap_or(occupancy)
            + (occupancy - self.occupancy.unwrap_or(occupancy)) / 64.0;
        self.occupancy = Some(smoothed);
        // One currently due packet plus target-delay packets ahead of it.
        let error = smoothed - (self.stats.target_delay.as_secs_f64() / SLOT.as_secs_f64() + 1.0);
        // Packet arrivals quantize occupancy by one frame. Ignore that bounded sawtooth
        // so the servo does not oppose the measured sub-packet clock-rate correction.
        let error = error.signum() * (error.abs() - 1.0).max(0.0);
        self.integral = (self.integral + error * 0.1).clamp(-900.0, 900.0);
        self.stats.drift_ppm = (self.rate_ppm + self.integral + error * 20.0)
            .clamp(-1000.0, 1000.0)
            .round() as i32;
        let step = 1.0 + f64::from(self.stats.drift_ppm) / 1_000_000.0;
        let needed = (self.phase + step * (AUDIO_FRAME_SAMPLES - 1) as f64).floor() as usize + 2;
        let mut result = AudioPull::Packet;
        // At most two 480-frame decodes: consumption is bounded to 481 frames per pull.
        while self.len < needed {
            let status = self.frame()?;
            result = match (result, status) {
                (_, AudioPull::Concealed) | (AudioPull::Concealed, _) => AudioPull::Concealed,
                (_, AudioPull::Fec) | (AudioPull::Fec, _) => AudioPull::Fec,
                _ => AudioPull::Packet,
            };
            let channels = self.kind.format().channels as usize;
            for frame in 0..AUDIO_FRAME_SAMPLES {
                let index = (self.head + self.len + frame) % RING_FRAMES;
                self.ring[index * channels..(index + 1) * channels]
                    .copy_from_slice(&self.scratch[frame * channels..(frame + 1) * channels]);
            }
            self.len += AUDIO_FRAME_SAMPLES;
        }
        let channels = self.kind.format().channels as usize;
        for frame in 0..AUDIO_FRAME_SAMPLES {
            let position = self.phase + frame as f64 * step;
            let index = position.floor() as usize;
            let fraction = (position - index as f64) as f32;
            for channel in 0..channels {
                let a = self.ring[((self.head + index) % RING_FRAMES) * channels + channel];
                let b = self.ring[((self.head + index + 1) % RING_FRAMES) * channels + channel];
                pcm[frame * channels + channel] = a + (b - a) * fraction;
            }
        }
        self.phase += step * AUDIO_FRAME_SAMPLES as f64;
        let consumed = self.phase.floor() as usize;
        self.phase -= consumed as f64;
        self.head = (self.head + consumed) % RING_FRAMES;
        self.len -= consumed;
        Ok(result)
    }

    fn frame(&mut self) -> Result<AudioPull, AudioError> {
        let mut result = AudioPull::Concealed;
        if let Some(index) = self.packets.iter().position(|p| p.seq == self.seq) {
            let packet = self.packets.swap_remove(index);
            if self
                .decoder
                .decode(Some(&packet.opus), false, &mut self.scratch)
                .is_ok()
            {
                self.starving = 0;
                result = AudioPull::Packet;
            }
        }
        if result == AudioPull::Concealed {
            self.stats.lost = self.stats.lost.saturating_add(1);
            self.loss_ns = (self.loss_ns + 1_000_000.0).min(20_000_000.0);
            self.target();
            self.starving = self.starving.saturating_add(1);
            // CELT-only packets (TOC bit 7) cannot carry SILK FEC. libopus falls back to PLC
            // when a valid SILK/hybrid following packet has no redundancy. Fec counts attempts,
            // not proof of recovery: the frozen wrapper exposes no LBRR-present query.
            if self.starving <= STARVATION
                && self.kind == AudioKind::Microphone
                && let Some(next) = self
                    .packets
                    .iter()
                    .find(|p| p.seq == self.seq.wrapping_add(1))
                && next.opus[0] & 0x80 == 0
                && validate_packet(&next.opus).is_ok()
                && self
                    .decoder
                    .decode(Some(&next.opus), true, &mut self.scratch)
                    .is_ok()
            {
                result = AudioPull::Fec;
                self.stats.fec = self.stats.fec.saturating_add(1);
            }
            if result == AudioPull::Concealed {
                self.stats.concealed = self.stats.concealed.saturating_add(1);
                if self.starving <= STARVATION {
                    if self.decoder.decode(None, false, &mut self.scratch).is_err() {
                        self.scratch.fill(0.0);
                    }
                } else {
                    if self.starving == STARVATION + 1 {
                        self.decoder.reset()?;
                    }
                    self.scratch.fill(0.0);
                }
            }
        }
        self.seq = self.seq.wrapping_add(1);
        Ok(result)
    }
}
