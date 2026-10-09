//! The engine's inputs and outputs. Frozen (WP-1.22).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crosspane_platform::{
    AudioEvent, CaptureEvent, CaptureId, CapturePortal, CaptureStart, CaptureTarget, ClipKinds,
    ClipboardEvent, HotkeyEvent, LocalPasteId, Overlay, OverlayEvent, OverlayId, Parked, PortalId,
    SessionEvent, StreamEndReason, StreamId, WindowEvent,
};
use crosspane_protocol::link::LinkEvent;
use crosspane_protocol::msg::{
    Capability, ClipFailure, ClipFetchId, ControlMessage, InputMessage, Placement, PointerMessage,
    Refusal,
};
use crosspane_protocol::projection::{
    BrowsableWindow, ParkingKind, ProjectionEndReason, ProxyPlacement,
};
use crosspane_types::ClipKind;
use crosspane_types::audio::{AudioKind, AudioStreamId};
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{PixelRect, PixelSize, PointDevice};
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::{DisplayId, NodeId, ProjectionId, WindowId};
use crosspane_types::input::{LockKeys, ScrollDelta};

/// Clipboard content in flight. `Debug` shows only the length, so the derived `Debug` of
/// `Input`/`Output` never prints content.
#[derive(Clone, PartialEq, Eq)]
pub struct ClipBytes(pub Vec<u8>);
impl std::fmt::Debug for ClipBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ClipBytes({} bytes)", self.0.len())
    }
}

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

/// One home transaction or home-related warp (WP-2.43): correlates [`Output::ReleaseAndWarp`]
/// and [`Output::HomeBind`] with their answers. Allocated by the engine, never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HomeOp(pub u64);

/// What became of the warp requested by [`Output::ReleaseAndWarp`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Warp {
    /// The capture (if any) was released and the warp was submitted.
    Done,
    /// The capture (if any) was released, but the warp was skipped because the platform's I/O
    /// gate was closed. The pointer is wherever it was.
    Skipped,
}

/// Why home (WP-2.43) was not entered or was left without a crossing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HomeFailure {
    /// An injector this node owns did not confirm its releases in time; the capture stayed live.
    Drain,
    /// The home bind could not be installed, or was lost and could not be reinstalled.
    Bind,
    /// Releasing the capture failed or timed out.
    Release,
    /// The capture was released but the pointer could not be warped (gate closed).
    Warp,
    /// The window did not take focus in time.
    Focus,
    /// A guard closed during entry (a button was pressed, the gate closed, crossing was disarmed).
    Guard,
    /// The projection, its placement or its last usable pointer exit went away.
    Gone,
}

/// Why a `SetPortals` was not installed (WP-2.43).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PortalsFailure {
    /// The backend refused the new set; the previous set and any active capture are intact.
    Rejected,
    /// The call timed out or the backend shut down; whether the previous set and the capture
    /// survive is unknown.
    Uncertain,
}

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
    /// E2: project this node's `window` to `to`.
    Project {
        window: WindowId,
        to: NodeId,
        place: Option<ProxyPlacement>,
    },
    /// E2: end a projection (either role) and return the window to its source.
    Return(ProjectionKey),
    /// Return a projection, placing the restored content on its source's display.
    ReturnAt(ProjectionKey, ProxyPlacement),
    /// E2 (WP-2.13): ask `peer` for the windows this node may pull. The answer comes back as
    /// `Output::BrowseResult` with the same `request` (the caller's correlation number).
    Browse { peer: NodeId, request: u32 },
    /// E2 (WP-2.13): ask `peer` to project its `window` here. Success shows up as a normal
    /// projection; a refusal as `Output::BrowseResult { result: Err(..) }` with this `request`.
    Pull {
        peer: NodeId,
        window: WindowId,
        request: u32,
    },
}

/// Names one projection anywhere in the workspace: projection ids are unique per source node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProjectionKey {
    pub source: NodeId,
    pub projection: ProjectionId,
}

/// Identifies one audio session on an authenticated connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AudioKey {
    pub peer: NodeId,
    pub stream: AudioStreamId,
    /// Local monotonically increasing admission generation; never reused, including reconnects.
    /// Not sent on the wire. Async callbacks and device handles must retain the complete key.
    pub generation: u64,
}

/// Which PCM endpoint the agent binds to the codec/network worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AudioEndpoint {
    VirtualSpeaker,
    VirtualMicrophone,
    LocalCapture,
    LocalPlayback,
}

/// What happened on a proxy window (E2 destination).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ProxyEvent {
    /// The proxy entered (`true`) or left fullscreen on its display. Reported before the `Resized`
    /// of the same change, whether the host did it (`Output::ProxyFullscreen`) or the user did.
    Fullscreen(bool),
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
    /// Where the content area is (WP-2.43): its top-left at `origin` device pixels on this node's
    /// `display`, `size` device pixels. `display: None`: on no display right now (minimised,
    /// fully occluded, or the host can't tell). Reported after `Resized` on open and on every
    /// change.
    Placed {
        display: Option<DisplayId>,
        origin: PointDevice,
        size: PixelSize,
    },
}

#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Input {
    /// CLIP-v0: after `PeerUp`, only when both `Hello`s carry `CLIP_FEATURE`; false removes it
    /// (link loss, or a `HelloRefresh` without the feature).
    ClipPeer {
        peer: NodeId,
        available: bool,
    },
    /// A local clipboard event from `ClipboardHost` (CLIP-v0 §5).
    Clipboard(ClipboardEvent),
    /// The agent's answer to `Output::ClipRead`: the content for `fetch`, or why not.
    ClipReadDone {
        peer: NodeId,
        fetch: ClipFetchId,
        result: Result<ClipBytes, ClipFailure>,
    },
    /// A complete clip data stream from `peer`. The agent has already checked the header (kind
    /// cap, exact length, no extra bytes) and reset any stream that failed.
    ClipData {
        peer: NodeId,
        fetch: ClipFetchId,
        kind: ClipKind,
        data: ClipBytes,
    },
    /// A timer fired: deliver at or after [`crate::Engine::next_deadline`].
    Tick,
    // ---- Audio (D8) ----
    Audio(AudioEvent),
    /// Delivered after PeerUp, only when Hello negotiated the audio feature. False removes it.
    AudioPeer {
        peer: NodeId,
        name: String,
        available: bool,
    },
    /// Physical device open completion. Cancelled or expired opens must be closed on arrival.
    AudioDeviceOpened {
        key: AudioKey,
        kind: AudioKind,
        result: Result<(), Failure>,
    },
    Capture(CaptureEvent),
    /// The result of [`Output::BeginCapture`].
    CaptureBegun {
        id: CaptureId,
        result: Result<CaptureStart, Failure>,
    },
    /// Host acknowledgement for exactly the requested drag continuation arm.
    DragArmed {
        key: ProjectionKey,
        token: u32,
        ok: bool,
    },
    Hotkey(HotkeyEvent),
    Session(SessionEvent),
    Overlay(OverlayEvent),
    Link(LinkEvent),
    /// A peer link is up (authenticated, `Hello` exchanged).
    PeerUp {
        peer: NodeId,
    },
    /// After `PeerUp`, only when both `Hello`s carry `DRAG_FEATURE`; false removes it on link
    /// loss or a `HelloRefresh` without the feature.
    DragPeer {
        peer: NodeId,
        available: bool,
    },
    /// Whether `peer` negotiated `DRAG_IN_FEATURE` (both Hellos), like `DragPeer` for `drag/0`.
    DragInPeer {
        peer: NodeId,
        available: bool,
    },
    /// The native window id of an open proxy, so a moved proxy can be recognised.
    ProxyWindow {
        key: ProjectionKey,
        window: WindowId,
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
    /// The user-visible local microphone indicator is actually shown for this admitted request.
    AudioIndicatorShown {
        key: AudioKey,
        visible: bool,
    },
    /// The peer's connection was silently replaced (`LinkEvent::HelloRefresh`; WP-3.0b): end every
    /// audio session with it, pending or active, keeping its stream-ID allocation, replay
    /// high-water, admission generations and retained device demand.
    AudioConnectionReplaced {
        peer: NodeId,
    },
    /// The agent's audio worker failed this exact session; only the matching complete key ends.
    AudioStreamFailed {
        key: AudioKey,
    },
    /// Whether this node can host microphones at all. Defaults to false; production speaker-v0
    /// never sets it true, so microphone admissions are refused without opening any device.
    AudioMicrophoneSupport {
        available: bool,
    },
    // ---- E2 ----
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
    /// The result of [`Output::SetPortals`] (WP-2.43), one per `SetPortals` in order: `ids` are
    /// the portals the agent asked the backend to install (empty when crossing is off or there
    /// is no capture backend; then `result` is `Err`).
    PortalsSet {
        ids: Vec<PortalId>,
        result: Result<(), PortalsFailure>,
    },
    /// The result of [`Output::ReleaseAndWarp`] with the same `op`.
    CaptureReleased {
        op: HomeOp,
        result: Result<Warp, Failure>,
    },
    /// The result of [`Output::HomeBind`] with the same `op` and `install`. While a bind is
    /// installed, the agent also delivers `install: true, result: Err(..)` whenever it finds the
    /// bind missing and cannot reinstall it.
    HomeBindSet {
        op: HomeOp,
        install: bool,
        result: Result<(), Failure>,
    },
    /// Fallback cursor only (WP-2.43 phase 2): this node's physical pointer on one of its
    /// displays while the controller is home in a projected window. The agent polls it only then.
    LocalPointer {
        display: DisplayId,
        position: PointDevice,
    },
}

#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Output {
    /// `ClipboardHost::promise(offer, kinds)`. `offer` is the engine's local promise number, not
    /// the peer's `ClipOfferId`.
    ClipPromise {
        offer: u64,
        kinds: ClipKinds,
    },
    /// `ClipboardHost::withdraw(offer)`.
    ClipWithdraw {
        offer: u64,
    },
    /// `ClipboardHost::fulfil(paste, data)`. Backends ignore an unknown or already answered paste.
    ClipFulfil {
        paste: LocalPasteId,
        data: Option<ClipBytes>,
    },
    /// `ClipboardHost::read(kind, max_bytes)` to answer `peer`'s admitted fetch; the agent replies
    /// with `Input::ClipReadDone`. Never emitted except for an admitted fetch.
    ClipRead {
        peer: NodeId,
        fetch: ClipFetchId,
        kind: ClipKind,
        max_bytes: usize,
    },
    /// Open a clip data stream to `peer`: `ClipDataHeader` and exactly `data`, lowest priority.
    SendClipData {
        peer: NodeId,
        fetch: ClipFetchId,
        kind: ClipKind,
        data: ClipBytes,
    },
    // ---- Audio (D8) ----
    AddAudioPeer {
        peer: NodeId,
        name: String,
    },
    RemoveAudioPeer {
        peer: NodeId,
    },
    /// The agent opens a mono 48 kHz physical capture and replies with AudioDeviceOpened.
    OpenAudioCapture {
        key: AudioKey,
    },
    CloseAudioCapture {
        key: AudioKey,
    },
    /// The agent opens stereo 48 kHz physical playback and replies with AudioDeviceOpened.
    OpenAudioPlayback {
        key: AudioKey,
    },
    CloseAudioPlayback {
        key: AudioKey,
    },
    /// Start only after admission and endpoint acquisition. No microphone samples precede it.
    StartAudioStream {
        key: AudioKey,
        kind: AudioKind,
        endpoint: AudioEndpoint,
    },
    StopAudioStream {
        key: AudioKey,
    },
    /// Complete local physical-device usage state. Show before opening capture; clear after stop.
    AudioIndicators {
        microphones: Vec<AudioKey>,
        speakers: Vec<AudioKey>,
    },
    SetPortals(Vec<CapturePortal>),
    /// Turn `InputCapture::set_monitor_local_activity` on or off (local override on a target).
    MonitorLocalActivity(bool),
    BeginCapture {
        id: CaptureId,
        portal: PortalId,
        /// WP-2.43: `true` only for the capture begun for a home exit; the agent then feeds every
        /// event queued during `begin()` to the engine before it delivers `CaptureBegun`.
        /// Ordinary captures keep today's delivery order.
        drain_first: bool,
    },
    /// `InputCapture::begin_drag`; answered by `CaptureBegun`, after queued physical events.
    BeginDrag {
        id: CaptureId,
        portal: PortalId,
        button: MouseButton,
    },
    /// Install a host arm, acknowledged by `DragArmed`; an unused arm expires at `until`.
    ArmDrag {
        key: ProjectionKey,
        token: u32,
        until: crosspane_types::time::MonoTime,
    },
    DisarmDrag {
        key: ProjectionKey,
        token: u32,
    },
    EndCapture {
        warp_to: Option<(DisplayId, PointDevice)>,
    },
    /// WP-2.43: `InputCapture::end(Some(warp_to))`: end the capture if one is live, a plain warp
    /// otherwise. Answered with [`Input::CaptureReleased`] carrying `op`.
    ReleaseAndWarp {
        op: HomeOp,
        warp_to: (DisplayId, PointDevice),
    },
    /// WP-2.43 §2.9: install (`true`) or remove (`false`) the home bind. Answered with
    /// [`Input::HomeBindSet`] carrying `op` and `install`. Removal is idempotent.
    HomeBind {
        op: HomeOp,
        install: bool,
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
    // ---- E2 source ----
    /// `WindowParking::park` with the destination's content size and scale (from `Accepted`);
    /// answer with `Input::Parked`.
    Park {
        window: WindowId,
        size: PixelSize,
        scale: f64,
    },
    /// `WindowParking::set_fullscreen(window, fullscreen)` then `WindowParking::resize(window,
    /// size, scale)`; one answer, `Input::Parked`, with the real state in `Parked::fullscreen`.
    ResizeParked {
        window: WindowId,
        size: PixelSize,
        scale: f64,
        fullscreen: bool,
    },
    /// Put the proxy into, or take it out of, fullscreen on its current display.
    ProxyFullscreen {
        key: ProjectionKey,
        fullscreen: bool,
    },
    /// `WindowParking::restore`.
    Restore {
        window: WindowId,
        /// `Some` means `WindowParking::restore_at`.
        place: Option<ProxyPlacement>,
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
        place: Option<ProxyPlacement>,
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
    /// E2 (WP-2.13): `peer`'s answer to `Command::Browse` or `Command::Pull` number `request`. A
    /// pull that succeeds has no result here (its projection starts instead).
    BrowseResult {
        peer: NodeId,
        request: u32,
        result: Result<Vec<BrowsableWindow>, Refusal>,
    },
}

/// Why this node's controller session was ended by `release()` (WP-4.5): the release chord, or a
/// release command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReleaseCause {
    /// The release chord: seen in the captured stream (including while a home exit was being
    /// activated) or reported by the platform's hotkey.
    Chord,
    /// `Command::ReleaseControl` (`crosspanectl release`, the tray's "Take input back").
    Command,
}

/// Something to tell the user (tray notification, HUD text).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Notice {
    MicInUseBy(NodeId),
    SpeakerInUseBy(NodeId),
    AudioRefused {
        peer: NodeId,
        kind: AudioKind,
        reason: Refusal,
    },
    LostConnection(NodeId),
    TargetLocked(NodeId),
    Refused {
        peer: NodeId,
        reason: Refusal,
    },
    ControlledBy(NodeId),
    ControlEnded(NodeId),
    /// The target was used locally; its controller ended the session and returned control home.
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
    /// WP-2.43: this node's input went home into its own projected window (`entered`), or left it.
    Home {
        key: ProjectionKey,
        entered: bool,
    },
    /// WP-2.43: home could not be entered, or was left without a crossing; the E1 session is in
    /// a safe state (captured as before, or ended).
    HomeFailed {
        key: ProjectionKey,
        reason: HomeFailure,
    },
    /// WP-4.5: this node, as controller, ended its session with `peer` by a release (`cause`).
    /// Emitted exactly once per ended controller session, and only if a session was active (a
    /// release with none emits nothing). Not for display: the other ways a session ends have
    /// their own notices (or none).
    ControlReleased {
        peer: NodeId,
        cause: ReleaseCause,
    },
}
