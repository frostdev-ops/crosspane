//! The engine's inputs and outputs. Frozen (WP-1.22).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crosspane_platform::{
    CaptureEvent, CaptureId, CapturePortal, CaptureStart, CaptureTarget, HotkeyEvent, Overlay,
    OverlayEvent, OverlayId, Parked, PortalId, SessionEvent, StreamEndReason, StreamId,
    WindowEvent,
};
use crosspane_protocol::link::LinkEvent;
use crosspane_protocol::msg::{
    Capability, ControlMessage, InputMessage, Placement, PointerMessage, Refusal,
};
use crosspane_protocol::projection::{ParkingKind, ProjectionEndReason};
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{PixelRect, PixelSize, PointDevice};
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::{DisplayId, NodeId, ProjectionId, WindowId};
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
    /// E2: project this node's `window` to `to` (docs/wp/E2-v0.md).
    Project { window: WindowId, to: NodeId },
    /// E2: end a projection (either role) and return the window to its source.
    Return(ProjectionKey),
}

/// Names one projection anywhere in the workspace: projection ids are unique per source node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProjectionKey {
    pub source: NodeId,
    pub projection: ProjectionId,
}

/// What happened on a proxy window (E2 destination).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ProxyEvent {
    /// The content area changed: `size` device pixels at `scale` device pixels per logical unit.
    Resized {
        size: PixelSize,
        scale: f64,
    },
    Focus(bool),
    /// The user closed the proxy (return the window).
    CloseRequested,
    /// The proxy window went away without a request (the agent's window host failed).
    Lost,
    Key {
        usage: HidUsage,
        down: bool,
    },
    /// `position`: device pixels of the content, origin top-left.
    Button {
        button: MouseButton,
        down: bool,
        position: PointDevice,
    },
    Scroll {
        delta: ScrollDelta,
        position: PointDevice,
    },
    Motion {
        position: PointDevice,
    },
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
    // ---- E2 (docs/wp/E2-v0.md) ----
    /// This node's windows (from `WindowSource`).
    Windows(WindowEvent),
    /// The result of `Output::Park` or `Output::ResizeParked`.
    Parked {
        window: WindowId,
        result: Result<Parked, Failure>,
    },
    /// The result of `Output::StartCapture`.
    CaptureStarted {
        projection: ProjectionId,
        result: Result<StreamId, Failure>,
    },
    /// A capture stream ended (from `FrameCapture`).
    CaptureEnded {
        stream: StreamId,
        reason: StreamEndReason,
    },
    /// The result of `Output::OpenProxy`: the proxy's content area.
    ProxyOpened {
        key: ProjectionKey,
        result: Result<(PixelSize, f64), Failure>,
    },
    Proxy {
        key: ProjectionKey,
        event: ProxyEvent,
    },
    /// The destination's decoder couldn't apply a frame (ask for a key frame).
    MediaError {
        key: ProjectionKey,
    },
}

#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Output {
    SetPortals(Vec<CapturePortal>),
    /// Turn `InputCapture::set_monitor_local_activity` on or off (local override on a target).
    MonitorLocalActivity(bool),
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
    // ---- E2 source (docs/wp/E2-v0.md) ----
    /// `WindowParking::park` with the destination's content size and scale (from `Accepted`);
    /// answer with `Input::Parked`.
    Park {
        window: WindowId,
        size: PixelSize,
        scale: f64,
    },
    /// `WindowParking::resize` (from `Resize`); answer with `Input::Parked`.
    ResizeParked {
        window: WindowId,
        size: PixelSize,
        scale: f64,
    },
    /// `WindowParking::restore`.
    Restore {
        window: WindowId,
    },
    /// `WindowSource::activate` (focus guard before keys, 03 §4.5).
    ActivateWindow {
        window: WindowId,
    },
    /// `FrameCapture::start`; the agent feeds the frames to this projection's encoder and media
    /// stream to `peer`, and answers with `Input::CaptureStarted`.
    StartCapture {
        projection: ProjectionId,
        peer: NodeId,
        target: CaptureTarget,
        crop: Option<PixelRect>,
        max_fps: u32,
    },
    SetCaptureCrop {
        stream: StreamId,
        crop: Option<PixelRect>,
    },
    StopCapture {
        stream: StreamId,
    },
    /// The next frame this projection's encoder produces is a key frame.
    RequestKeyFrame {
        projection: ProjectionId,
    },
    // ---- E2 destination ----
    /// Open a proxy window with roughly `size` device pixels of content; answer with
    /// `Input::ProxyOpened`. Frames for `key` arriving on media streams go to this proxy.
    OpenProxy {
        key: ProjectionKey,
        title: String,
        app_id: String,
        size: PixelSize,
    },
    /// The source's actual content size and parking (resize the proxy to it if it differs:
    /// the app refused a size).
    ProxyGeometry {
        key: ProjectionKey,
        size: PixelSize,
        parking: ParkingKind,
    },
    ProxyTitle {
        key: ProjectionKey,
        title: String,
    },
    CloseProxy {
        key: ProjectionKey,
    },
}

/// Something to tell the user (tray notification, HUD text).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Notice {
    LostConnection(NodeId),
    TargetLocked(NodeId),
    Refused {
        peer: NodeId,
        reason: Refusal,
    },
    ControlledBy(NodeId),
    ControlEnded(NodeId),
    LocalOverride(NodeId),
    Panic,
    /// E2.
    ProjectionStarted {
        key: ProjectionKey,
        peer: NodeId,
        parking: ParkingKind,
    },
    ProjectionEnded {
        key: ProjectionKey,
        reason: ProjectionEndReason,
    },
    ProjectionRefused {
        peer: NodeId,
        reason: Refusal,
    },
}
