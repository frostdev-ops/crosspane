//! Shared-audio format and identifiers (D8, WP-3.0).

/// Samples per channel per second.
pub const AUDIO_RATE: u32 = 48_000;
/// Sample frames per channel in one ten-millisecond packet.
pub const AUDIO_FRAME_SAMPLES: usize = 480;
/// Maximum encoded Opus packet size, before framing.
pub const MAX_OPUS_PACKET: usize = 400;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AudioKind {
    Speaker,
    Microphone,
}

impl AudioKind {
    pub const fn format(self) -> AudioFormat {
        AudioFormat {
            rate: AUDIO_RATE,
            channels: match self {
                Self::Speaker => 2,
                Self::Microphone => 1,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AudioFormat {
    pub rate: u32,
    pub channels: u16,
}

impl AudioFormat {
    pub const fn is_valid(self) -> bool {
        self.rate == AUDIO_RATE && (self.channels == 1 || self.channels == 2)
    }

    pub const fn frame_samples(self) -> usize {
        AUDIO_FRAME_SAMPLES * self.channels as usize
    }
}

/// Unique across both initiators for a peer. Zero is invalid. The smaller NodeId allocates odd
/// IDs and the larger allocates even IDs. Since WP-3.0b an ID is never reused for a peer for the
/// lifetime of the agent process, across connection replacement and reconnects alike (exhaustion
/// refuses, never wraps). Counters are not durable: a restarted peer starts over.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AudioStreamId(pub u16);
