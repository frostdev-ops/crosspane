//! The fixed Crosspane loopback inventory (WP-3.0b/3.4) and the checks that decide whether the
//! installed devices are exactly that inventory, and whether a physical default output is one the
//! host may play on.

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
    Rate,
    Format,
}

impl Reject {
    pub(super) fn reason(self) -> &'static str {
        match self {
            Reject::CrosspaneDevice => "the default output is a Crosspane virtual device",
            Reject::NotAlive => "the default output device is not alive",
            Reject::NoOutput => "the default output device has no single output stream",
            Reject::Rate => "the default output is not running at 48 kHz",
            Reject::Format => "the default output is not 48 kHz float32 stereo",
        }
    }
}

/// Accept a physical default output only when its native client-facing format is exactly 48 kHz
/// float32 stereo, one stream, interleaved or planar. Nothing is ever converted or changed.
pub(super) fn playback_layout(info: &DeviceInfo) -> Result<OutputLayout, Reject> {
    if is_crosspane_uid(&info.uid) {
        return Err(Reject::CrosspaneDevice);
    }
    if !info.alive {
        return Err(Reject::NotAlive);
    }
    let [StreamInfo { format, .. }] = info.output_streams.as_slice() else {
        return Err(Reject::NoOutput);
    };
    if info.nominal_rate != RATE || format.sample_rate != RATE {
        return Err(Reject::Rate);
    }
    if *format == StreamFormat::float32(2, true) {
        Ok(OutputLayout::Interleaved)
    } else if *format == StreamFormat::float32(2, false) {
        Ok(OutputLayout::Planar)
    } else {
        Err(Reject::Format)
    }
}

// The accepted playback flag words, spelled out so a reader can check them against the SDK.
const _: () = {
    assert!(FLAGS_FLOAT_PACKED == 9);
    assert!(FLAGS_FLOAT_PACKED | FLAG_NON_INTERLEAVED == 41);
    assert!(FORMAT_LINEAR_PCM == 0x6c70_636d);
};
