//! The logical message model. Field meanings are frozen; encodings live in [`crate::wire`].

use crosspane_types::ClipKind;
use crosspane_types::audio::{AudioKind, AudioStreamId};
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{PointDevice, PointMm};
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::{DisplayId, NodeId, SessionId};
use crosspane_types::input::{LockKeys, ScrollDelta};

// ---------------------------------------------------------------------------------------------
// Input channel: reliable, ordered, highest priority (03 §2 "Input-discrete").
// ---------------------------------------------------------------------------------------------

/// Messages on the input-discrete stream. `seq` numbers every controller-to-target message of one
/// session in one increasing sequence, starting at 1; the target acknowledges with [`Self::Ack`].
#[derive(Clone, Debug, PartialEq)]
pub enum InputMessage {
    /// Controller → target: `button` down at `position` on `display` (DRAG-v0 D-5). The target moves
    /// its pointer there, then handles it exactly as `Button { button, down: true }` (lease, journal,
    /// heartbeat); its up is an ordinary `Button` up. Sent only when `DRAG_FEATURE` was negotiated.
    PressAt {
        session: SessionId,
        seq: u32,
        button: MouseButton,
        display: DisplayId,
        position: PointDevice,
    },
    /// Controller → target: a physical key changed state.
    Key {
        session: SessionId,
        seq: u32,
        usage: HidUsage,
        down: bool,
    },
    /// Controller → target: a pointer button changed state.
    Button {
        session: SessionId,
        seq: u32,
        button: MouseButton,
        down: bool,
    },
    /// Controller → target: a scroll step.
    Scroll {
        session: SessionId,
        seq: u32,
        delta: ScrollDelta,
    },
    /// Controller → target: lock-key state to apply (04 §8: synchronised on entry).
    LockKeys {
        session: SessionId,
        seq: u32,
        keys: LockKeys,
    },
    /// Controller → target: the heartbeat (04 §8 invariant 2). Lists every key and button the
    /// controller believes is held on this target; sent every 50 ms while anything is held, every
    /// 250 ms otherwise. At most [`MAX_HELD_KEYS`] keys.
    State {
        session: SessionId,
        seq: u32,
        held_keys: Vec<HidUsage>,
        held_buttons: Vec<MouseButton>,
    },
    /// Target → controller: every message up to and including `seq` has been processed.
    Ack { session: SessionId, seq: u32 },
    /// Target → controller: a change in the target's state for this session.
    Status {
        session: SessionId,
        status: TargetStatus,
    },
    /// E2: destination → source input for a projected window (docs/wp/E2-v0.md).
    Proj(crate::projection::ProjInput),
}

/// The most keys a [`InputMessage::State`] heartbeat may list. A controller holding more releases
/// the excess: no human holds more than this many keys on purpose.
pub const MAX_HELD_KEYS: usize = 32;

/// What a target reports about a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TargetStatus {
    /// Physical input on the target paused injection (04 §6 local override).
    LocalOverride,
    /// Injection resumed after a local override.
    Resumed,
    /// The target refuses injection for this reason; the controller takes input back.
    Refused(Refusal),
}

/// Why a node refuses a session or a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Refusal {
    /// The granting node hasn't given the requester the needed permission (04 §2).
    Permission,
    /// The node is locked, inactive or in an unknown session state (04 §7).
    Locked,
    /// Secure keyboard input is active on the node.
    SecureInput,
    /// The node is busy (e.g. another controller holds it).
    Busy,
    /// The node's injector failed.
    InjectorFailed,
}

// ---------------------------------------------------------------------------------------------
// Motion: unreliable datagrams, latest wins (03 §2 "Input-motion").
// ---------------------------------------------------------------------------------------------

/// Controller → target: an absolute pointer position in the target's coordinates. `seq` is a
/// separate sequence from [`InputMessage`]'s; the target drops any datagram not newer than the last
/// one it applied.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerMessage {
    pub session: SessionId,
    pub seq: u32,
    pub display: DisplayId,
    pub position: PointDevice,
}

// ---------------------------------------------------------------------------------------------
// Control channel: reliable, ordered (03 §2 "Control").
// ---------------------------------------------------------------------------------------------

/// Messages on the control stream of an established (paired, authenticated) connection.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ControlMessage {
    /// The first message each side sends.
    Hello(Hello),
    /// The sender's displays changed (full snapshot).
    Displays(Vec<DisplayInfo>),
    /// Placements of displays on the shared layout canvas (03 §1 "Shared configuration").
    Layout(Vec<Placement>),
    /// Controller → target: start controlling the target, with the pointer entering at `entry`.
    StartControl {
        session: SessionId,
        entry_display: DisplayId,
        entry: PointDevice,
        lock_keys: LockKeys,
    },
    /// Target → controller: the session is accepted; input may flow.
    ControlStarted {
        session: SessionId,
    },
    /// Target → controller: the session is refused.
    ControlRefused {
        session: SessionId,
        reason: Refusal,
    },
    /// Either side: the session is over. The target releases everything it injected for it.
    EndControl {
        session: SessionId,
        reason: EndReason,
    },
    /// The sender changed what it grants the receiver (informational; enforcement is local, 04 §2).
    Grants(Vec<Capability>),
    /// A signed revocation of a lost or stolen device (04 §4).
    Revocation(RevocationNotice),
    /// Clock-offset estimation (03 §8): the receiver answers with `Pong`.
    Ping {
        t0: u64,
    },
    /// `t0` echoed; `t1` = receive time and `t2` = send time on the responder's clock (ns).
    Pong {
        t0: u64,
        t1: u64,
        t2: u64,
    },
    /// The sender is about to close the connection; `message` is for logs and UI.
    Goodbye {
        message: String,
    },
    /// E2 window projection (docs/wp/E2-v0.md).
    Projection(crate::projection::ProjectionMessage),
    /// D8: feature-negotiated audio; channels must equal kind.format().channels.
    AudioOpen {
        stream: AudioStreamId,
        kind: AudioKind,
        channels: u8,
    },
    AudioOpened {
        stream: AudioStreamId,
    },
    AudioRefused {
        stream: AudioStreamId,
        reason: Refusal,
    },
    AudioClose {
        stream: AudioStreamId,
    },
    ClipOffer(ClipOffer),
    ClipWithdraw(ClipWithdraw),
    ClipFetch(ClipFetch),
    ClipFetchFailed(ClipFetchFailed),
}

/// Why an E1 session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EndReason {
    /// The pointer crossed back, or the user pressed the release hotkey.
    Released,
    /// Panic (04 §6).
    Panic,
    /// The target became locked, inactive or asleep.
    TargetLocked,
    /// The controller locked or slept.
    ControllerLocked,
    /// Heartbeats or acknowledgements stopped (04 §8 invariant 3).
    LinkLost,
    /// The permission was revoked.
    Revoked,
}

/// Exchanged at the start of every connection.
#[derive(Clone, Debug, PartialEq)]
pub struct Hello {
    /// The sender's [`crate::PROTOCOL_MINOR`].
    pub minor: u32,
    /// The user-visible device name.
    pub name: String,
    /// Optional features the sender supports, by name (e.g. `"e1"`). Unknown names are ignored.
    pub features: Vec<String>,
    pub displays: Vec<DisplayInfo>,
}

/// Where one display sits on the layout canvas. Each node is authoritative for its own displays;
/// conflicting edits resolve last-writer-wins by `(version, node)` (03 §1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub node: NodeId,
    pub display: DisplayId,
    /// Top-left corner on the canvas, in millimetres.
    pub origin: PointMm,
    pub version: u64,
}

/// A capability one node grants another (04 §2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Capability {
    /// The peer may inject input here.
    InputAccept,
    /// Windows the user sends to the peer may be streamed to it.
    WindowShare,
    /// The peer may list this node's windows and pull them.
    WindowBrowse,
    /// The peer may open proxy windows here.
    WindowPresent,
    /// The peer may play sound on this node's default speakers. Default off.
    AudioSpeaker,
    /// The peer may capture this node's default microphone. Default off.
    AudioMic,
    /// The peer may read this node's clipboard when the user pastes on the peer. Default off.
    ClipboardRead, // wire 7
    /// The peer may place clipboard offers (promises) here. Default off.
    ClipboardWrite, // wire 8
}

/// Unique per holder, strictly increasing, never reused (CLIP-v0 §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ClipOfferId(pub u64);

/// Unique per requester, strictly increasing (CLIP-v0 §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ClipFetchId(pub u64);

/// An offer contains 1..=2 distinct kinds and no clipboard content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipOffer {
    pub offer: ClipOfferId,
    pub kinds: Vec<ClipKind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipWithdraw {
    pub offer: ClipOfferId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipFetch {
    pub fetch: ClipFetchId,
    pub offer: ClipOfferId,
    pub kind: ClipKind,
}

/// Wire codes 1..=5 in declaration order; 0 and unknown codes are rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipFailure {
    Expired,
    Locked,
    NotGranted,
    TooLarge,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipFetchFailed {
    pub fetch: ClipFetchId,
    pub reason: ClipFailure,
}

/// "Node `issuer` revokes `revoked`" (04 §4), signed by the issuer's device key over
/// [`RevocationNotice::signed_bytes`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevocationNotice {
    pub revoked: NodeId,
    pub issuer: NodeId,
    /// Unix time in milliseconds when the issuer created the notice.
    pub issued_at_ms: u64,
    /// ECDSA P-256 / SHA-256 signature, ASN.1 DER.
    pub signature: Vec<u8>,
}

impl RevocationNotice {
    /// The exact bytes the signature covers: a fixed context string, then the revoked ID, the
    /// issuer ID and the time as little-endian u64.
    pub fn signed_bytes(revoked: &NodeId, issuer: &NodeId, issued_at_ms: u64) -> Vec<u8> {
        const CONTEXT: &[u8] = b"crosspane revocation v1\0";
        let mut v = Vec::with_capacity(CONTEXT.len() + 32 + 32 + 8);
        v.extend_from_slice(CONTEXT);
        v.extend_from_slice(&revoked.0);
        v.extend_from_slice(&issuer.0);
        v.extend_from_slice(&issued_at_ms.to_le_bytes());
        v
    }
}
