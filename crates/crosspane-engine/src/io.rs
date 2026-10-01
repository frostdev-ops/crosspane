//! The engine's inputs and outputs. Frozen (WP-1.22).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crosspane_platform::{
    CaptureEvent, CaptureId, CapturePortal, CaptureStart, HotkeyEvent, Overlay, OverlayEvent,
    OverlayId, PortalId, SessionEvent,
};
use crosspane_protocol::link::LinkEvent;
use crosspane_protocol::msg::{
    Capability, ControlMessage, InputMessage, Placement, PointerMessage, Refusal,
};
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::PointDevice;
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::{DisplayId, NodeId};
use crosspane_types::input::{LockKeys, ScrollDelta};

/// The HUD on the controller: "Input → ⟨target⟩" (04 §5).
pub const HUD: OverlayId = OverlayId(1);
/// The indicator on a target: "Controlled from ⟨controller⟩ — ⟨release hotkey⟩" (04 §5).
pub const TARGET_INDICATOR: OverlayId = OverlayId(2);

/// A platform error, reduced to what the engine decides on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Failure {
    Locked,
    SecureInput,
    PointerButtonHeld,
    PermissionDenied,
    Other,
}

/// Identifies one injection request; the agent answers with [`Input::InjectDone`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InjectId(pub u64);

/// A request to the local injectors (`KeyInjector` / `PointerInjector`).
#[derive(Clone, Debug, PartialEq)]
pub enum InjectCmd {
    Key {
        usage: HidUsage,
        down: bool,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    MoveTo {
        display: DisplayId,
        position: PointDevice,
    },
    Scroll(ScrollDelta),
    LockKeys(LockKeys),
    /// `release_all` on both injectors.
    ReleaseAll,
    /// `recover_keys` and `recover_buttons` after a crash.
    Recover {
        keys: Vec<HidUsage>,
        buttons: Vec<MouseButton>,
    },
}

/// User commands from the tray, `crosspanectl` or the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Command {
    /// Give input back to this node now (same as the release chord).
    ReleaseControl,
    /// End everything and disarm until re-armed (04 §6).
    Panic,
    /// Re-arm crossing after a release or panic.
    Rearm,
}

#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Input {
    /// A timer fired: deliver at or after [`crate::Engine::next_deadline`].
    Tick,
    Capture(CaptureEvent),
    /// The result of [`Output::BeginCapture`].
    CaptureBegun {
        id: CaptureId,
        result: Result<CaptureStart, Failure>,
    },
    Hotkey(HotkeyEvent),
    Session(SessionEvent),
    Overlay(OverlayEvent),
    Link(LinkEvent),
    /// A peer link is up (authenticated, `Hello` exchanged).
    PeerUp {
        peer: NodeId,
    },
    /// A fresh round-trip estimate for a peer.
    PeerRtt {
        peer: NodeId,
        rtt: Duration,
    },
    /// This node's displays changed.
    LocalDisplays(Vec<DisplayInfo>),
    /// A peer's displays changed (from its `Hello` or `Displays`).
    PeerDisplays {
        peer: NodeId,
        displays: Vec<DisplayInfo>,
    },
    /// The merged layout: every node's display placements.
    Layout(Vec<Placement>),
    /// What this node grants each peer (enforced here, 04 §2).
    Grants(BTreeMap<NodeId, BTreeSet<Capability>>),
    Command(Command),
    /// The result of an [`Output::Inject`].
    InjectDone {
        id: InjectId,
        ok: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Output {
    SetPortals(Vec<CapturePortal>),
    BeginCapture {
        id: CaptureId,
        portal: PortalId,
    },
    EndCapture {
        warp_to: Option<(DisplayId, PointDevice)>,
    },
    ShowOverlay {
        id: OverlayId,
        overlay: Overlay,
    },
    HideOverlay(OverlayId),
    Inject {
        id: InjectId,
        cmd: InjectCmd,
    },
    SendInput {
        peer: NodeId,
        msg: InputMessage,
    },
    SendMotion {
        peer: NodeId,
        msg: PointerMessage,
    },
    SendControl {
        peer: NodeId,
        msg: ControlMessage,
    },
    /// Set the engine side of the I/O gate.
    EngineGate(bool),
    Notice(Notice),
}

/// Something to tell the user (tray notification, HUD text).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Notice {
    LostConnection(NodeId),
    TargetLocked(NodeId),
    Refused { peer: NodeId, reason: Refusal },
    ControlledBy(NodeId),
    ControlEnded(NodeId),
    LocalOverride(NodeId),
    Panic,
}
