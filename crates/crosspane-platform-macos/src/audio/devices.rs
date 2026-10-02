//! The fixed Crosspane loopback inventory (WP-3.0b/3.4) and the checks that decide whether the
//! installed devices are exactly that inventory, and whether a physical default output is one the
//! host may play on (any float32 stereo output whose rate the 48 kHz stream can be resampled to,
//! WP-3.7b).

use crosspane_media::audio::Resampler;

use super::hal::{
    CLASS_AUDIO_DEVICE, DeviceInfo, FLAG_NON_INTERLEAVED, FLAGS_FLOAT_PACKED, FORMAT_LINEAR_PCM,
    RATE, StreamFormat, StreamInfo, TRANSPORT_VIRTUAL,
};

/// Visible output the apps play into ("Crosspane speakers").
pub const SPEAKERS_APP_UID: &str = "io.frostdev.crosspane.audio.v0.speakers.app";
/// Hidden input the agent records the speakers' audio from.
pub const SPEAKERS_LOOPBACK_UID: &str = "io.frostdev.crosspane.audio.v0.speakers.loopback";
/// Visible input the apps record from ("Crosspane microphone"). Silent in v0.
pub const MIC_APP_UID: &str = "io.frostdev.crosspane.audio.v0.microphone.app";
/// Hidden output the agent would play the peer's microphone into. Never opened in v0.
pub const MIC_LOOPBACK_UID: &str = "io.frostdev.crosspane.audio.v0.microphone.loopback";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Scope {
    Input,
    Output,
}

/// What one of the four devices must look like.
#[derive(Clone, Copy, Debug)]
pub(super) struct Contract {
    pub uid: &'static str,
    /// The only scope with a stream.
    pub scope: Scope,
    pub channels: u32,
    pub hidden: bool,
}

pub(super) const SPEAKERS_APP: Contract = Contract {
    uid: SPEAKERS_APP_UID,
    scope: Scope::Output,
    channels: 2,
    hidden: false,
};
pub(super) const SPEAKERS_LOOPBACK: Contract = Contract {
    uid: SPEAKERS_LOOPBACK_UID,
    scope: Scope::Input,
    channels: 2,
    hidden: true,
};
pub(super) const MIC_APP: Contract = Contract {
    uid: MIC_APP_UID,
    scope: Scope::Input,
    channels: 1,
    hidden: false,
};
pub(super) const MIC_LOOPBACK: Contract = Contract {
    uid: MIC_LOOPBACK_UID,
    scope: Scope::Output,
    channels: 1,
    hidden: true,
};

/// The four contracts in the order of [`Devices`](super::Devices) fields.
pub(super) const CONTRACTS: [Contract; 4] =
    [SPEAKERS_APP, SPEAKERS_LOOPBACK, MIC_APP, MIC_LOOPBACK];

/// True for a UID that belongs to the Crosspane plug-in (any of the four, or any other UID in the
/// plug-in's namespace, so a future device can never become a playback target by accident).
pub(super) fn is_crosspane_uid(uid: &str) -> bool {
    uid.starts_with("io.frostdev.crosspane.audio.")
}

/// Exact validation of one installed device against its contract: read-back UID, object class,
/// virtual transport, alive, hidden flag, the one stream in the right scope (and none in the other)
/// with the complete ASBD, and the nominal sample rate.
pub(super) fn matches_contract(info: &DeviceInfo, contract: &Contract) -> bool {
    let (own, other) = match contract.scope {
        Scope::Input => (&info.input_streams, &info.output_streams),
        Scope::Output => (&info.output_streams, &info.input_streams),
    };
    info.uid == contract.uid
        && info.class_id == CLASS_AUDIO_DEVICE
        && info.transport == TRANSPORT_VIRTUAL
        && info.alive
        && info.hidden == contract.hidden
        && info.nominal_rate == RATE
        && other.is_empty()
        && own.len() == 1
        && own[0].format == StreamFormat::float32(contract.channels, true)
}

/// The buffer arrangement of a physical output's IOProc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OutputLayout {
    /// One buffer holding both channels, L R L R ...
    Interleaved,
    /// Two buffers of one channel each.
    Planar,
}

/// Why a default output cannot be played on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reject {
    CrosspaneDevice,
    NotAlive,
    NoOutput,
    /// The nominal rate and the stream's rate disagree, or the rate is one the host cannot
    /// resample the 48 kHz stream to.
    Rate,
    Format,
}

impl Reject {
    pub(super) fn reason(self) -> &'static str {
        match self {
            Reject::CrosspaneDevice => "the default output is a Crosspane virtual device",
            Reject::NotAlive => "the default output device is not alive",
            Reject::NoOutput => "the default output device has no single output stream",
            Reject::Rate => {
                "the default output's nominal and stream sample rates differ, or the rate is not supported"
            }
            Reject::Format => "the default output is not float32 stereo",
        }
    }
}

/// The sample rate of the Crosspane stream and of the virtual devices, in Hz: [`RATE`] as the
/// integer the resampler takes.
pub(super) const STREAM_RATE_HZ: u32 = 48_000;

/// The integer rate a physical output runs at, when the host can play the 48 kHz stream on it:
/// the nominal rate and the stream's virtual-format rate are the same whole number of hertz and
/// [`Resampler::new`] accepts the pair. A device whose two rates disagree is mid-change (or
/// misconfigured) and is never played on.
fn supported_rate(nominal: f64, stream: f64) -> Option<u32> {
    if nominal != stream || !nominal.is_finite() || nominal.fract() != 0.0 {
        return None;
    }
    if !(1.0..=f64::from(u32::MAX)).contains(&nominal) {
        return None;
    }
    let rate = nominal as u32;
    Resampler::new(STREAM_RATE_HZ, rate, 2).ok().map(|_| rate)
}

/// Accept a physical default output when its client-facing format is float32 stereo, one stream,
/// interleaved or planar, at a rate the host can resample the 48 kHz stream to (its nominal rate
/// equals the stream's rate; 48 kHz itself is the identity). Returns the buffer layout and that
/// rate in Hz. Nothing is ever converted or changed on the device.
pub(super) fn playback_layout(info: &DeviceInfo) -> Result<(OutputLayout, u32), Reject> {
    if is_crosspane_uid(&info.uid) {
        return Err(Reject::CrosspaneDevice);
    }
    if !info.alive {
        return Err(Reject::NotAlive);
    }
    let [StreamInfo { format, .. }] = info.output_streams.as_slice() else {
        return Err(Reject::NoOutput);
    };
    let rate = supported_rate(info.nominal_rate, format.sample_rate).ok_or(Reject::Rate)?;
    // The format must be float32 stereo at the device's own rate: compare it with the 48 kHz
    // reference formats after setting that rate.
    let at_rate = |reference: StreamFormat| StreamFormat {
        sample_rate: format.sample_rate,
        ..reference
    };
    if *format == at_rate(StreamFormat::float32(2, true)) {
        Ok((OutputLayout::Interleaved, rate))
    } else if *format == at_rate(StreamFormat::float32(2, false)) {
        Ok((OutputLayout::Planar, rate))
    } else {
        Err(Reject::Format)
    }
}

// The accepted playback flag words, spelled out so a reader can check them against the SDK, and the
// integer stream rate being the HAL's 48 kHz.
const _: () = {
    assert!(FLAGS_FLOAT_PACKED == 9);
    assert!(FLAGS_FLOAT_PACKED | FLAG_NON_INTERLEAVED == 41);
    assert!(FORMAT_LINEAR_PCM == 0x6c70_636d);
    assert!(STREAM_RATE_HZ as f64 == RATE);
};

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::audio::hal::StreamId;

    const BUILTIN_TRANSPORT: u32 = 0x626c_746e;

    /// A physical float32 stereo output at `rate` (device and stream agree).
    fn physical(rate: f64, interleaved: bool) -> DeviceInfo {
        DeviceInfo {
            uid: "BuiltInSpeakerDevice".to_string(),
            class_id: CLASS_AUDIO_DEVICE,
            transport: BUILTIN_TRANSPORT,
            alive: true,
            hidden: false,
            nominal_rate: rate,
            input_streams: vec![],
            output_streams: vec![StreamInfo {
                id: StreamId(2001),
                format: StreamFormat {
                    sample_rate: rate,
                    ..StreamFormat::float32(2, interleaved)
                },
            }],
        }
    }

    #[test]
    fn standard_rates_are_accepted_interleaved_and_planar() {
        for rate in [
            44_100u32, 48_000, 88_200, 96_000, 176_400, 192_000, 32_000, 22_050,
        ] {
            assert_eq!(
                playback_layout(&physical(f64::from(rate), true)),
                Ok((OutputLayout::Interleaved, rate)),
                "{rate} interleaved"
            );
            assert_eq!(
                playback_layout(&physical(f64::from(rate), false)),
                Ok((OutputLayout::Planar, rate)),
                "{rate} planar"
            );
        }
    }

    #[test]
    fn a_nominal_rate_that_differs_from_the_stream_rate_is_rejected() {
        let mut info = physical(44_100.0, true);
        info.output_streams[0].format.sample_rate = 48_000.0;
        assert_eq!(playback_layout(&info), Err(Reject::Rate));

        let mut info = physical(48_000.0, true);
        info.nominal_rate = 44_100.0;
        assert_eq!(playback_layout(&info), Err(Reject::Rate));

        // Even a fraction of a hertz is a device mid-change.
        let mut info = physical(44_100.0, false);
        info.output_streams[0].format.sample_rate = 44_100.5;
        assert_eq!(playback_layout(&info), Err(Reject::Rate));
    }

    #[test]
    fn an_unsupported_rate_is_rejected() {
        for rate in [
            // Outside the resampler's range.
            0.0,
            1.0,
            7_999.0,
            384_001.0,
            1.0e12,
            f64::NAN,
            f64::INFINITY,
            -44_100.0,
            // Whole-number rates whose ratio to 48 kHz needs more than 1024 phases.
            47_999.0,
            44_101.0,
            // Not a whole number of hertz.
            44_100.5,
        ] {
            let mut info = physical(44_100.0, true);
            info.nominal_rate = rate;
            info.output_streams[0].format.sample_rate = rate;
            assert_eq!(playback_layout(&info), Err(Reject::Rate), "{rate}");
        }
    }

    #[test]
    fn other_device_and_format_problems_are_still_rejected_at_any_rate() {
        for rate in [44_100.0, 48_000.0, 96_000.0] {
            let mut dead = physical(rate, true);
            dead.alive = false;
            assert_eq!(playback_layout(&dead), Err(Reject::NotAlive));

            let mut none = physical(rate, true);
            none.output_streams.clear();
            assert_eq!(playback_layout(&none), Err(Reject::NoOutput));

            let mut two = physical(rate, true);
            two.output_streams.push(two.output_streams[0]);
            assert_eq!(playback_layout(&two), Err(Reject::NoOutput));

            for edit in [
                (|f: &mut StreamFormat| f.channels_per_frame = 1) as fn(&mut StreamFormat),
                |f| f.channels_per_frame = 6,
                |f| {
                    f.format_flags = 12;
                    f.bits_per_channel = 16;
                    f.bytes_per_frame = 4;
                    f.bytes_per_packet = 4;
                },
                |f| f.bytes_per_frame = 16,
                |f| f.frames_per_packet = 4,
                |f| f.reserved = 1,
            ] {
                let mut info = physical(rate, true);
                edit(&mut info.output_streams[0].format);
                assert_eq!(playback_layout(&info), Err(Reject::Format), "{rate}");
            }
        }
    }

    #[test]
    fn a_crosspane_device_is_rejected_first_whatever_its_rate() {
        for rate in [44_100.0, 48_000.0, 96_000.0, 1.0] {
            for uid in [
                SPEAKERS_APP_UID,
                SPEAKERS_LOOPBACK_UID,
                MIC_APP_UID,
                MIC_LOOPBACK_UID,
                "io.frostdev.crosspane.audio.v1.future",
            ] {
                // Also not alive and with no stream: Crosspane still takes precedence.
                let mut info = physical(rate, true);
                info.uid = uid.to_string();
                assert_eq!(playback_layout(&info), Err(Reject::CrosspaneDevice));
                info.alive = false;
                info.output_streams.clear();
                assert_eq!(playback_layout(&info), Err(Reject::CrosspaneDevice));
            }
        }
    }

    #[test]
    fn the_virtual_device_contract_still_requires_48_khz() {
        let virtual_speakers = |nominal_rate: f64| DeviceInfo {
            uid: SPEAKERS_APP_UID.to_string(),
            class_id: CLASS_AUDIO_DEVICE,
            transport: TRANSPORT_VIRTUAL,
            alive: true,
            hidden: false,
            nominal_rate,
            input_streams: vec![],
            output_streams: vec![StreamInfo {
                id: StreamId(1001),
                format: StreamFormat::float32(2, true),
            }],
        };
        assert!(matches_contract(&virtual_speakers(48_000.0), &SPEAKERS_APP));
        for rate in [44_100.0, 96_000.0] {
            assert!(
                !matches_contract(&virtual_speakers(rate), &SPEAKERS_APP),
                "nominal {rate}"
            );
            let mut info = virtual_speakers(48_000.0);
            info.output_streams[0].format.sample_rate = rate;
            assert!(!matches_contract(&info, &SPEAKERS_APP), "stream {rate}");
        }
        assert_eq!(StreamFormat::float32(2, true).sample_rate, RATE);
    }

    #[test]
    fn no_reason_claims_a_48_khz_requirement() {
        for reject in [
            Reject::CrosspaneDevice,
            Reject::NotAlive,
            Reject::NoOutput,
            Reject::Rate,
            Reject::Format,
        ] {
            assert!(!reject.reason().contains("48 kHz"), "{reject:?}");
        }
    }
}
