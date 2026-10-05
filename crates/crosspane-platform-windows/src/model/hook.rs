//! Pure hook decisions; no native hook installation, suppression or secure-desktop claim.
//! \[E] Nonzero hook returns suppress; Windows 7+ silently removes timed-out hooks,
//! and Windows 10 1709+ caps LowLevelHooksTimeout at 1000ms:
//! <https://learn.microsoft.com/en-us/windows/win32/winmsg/lowlevelkeyboardproc>.
//! \[E] Keyboard callbacks precede asynchronous key-state updates; reconcile snapshots must
//! be acquired outside that callback, not mistaken for the event's updated state.
//! \[E] WM_HOTKEY is press-only; this independent physical chord tracker owns release:
//! <https://learn.microsoft.com/en-us/windows/win32/inputdev/wm-hotkey>.
//! \[U] scanCode may be zero or contain E0 in its high byte: the adapter masks to u8.
//! \[U] Suppressing a hook also blocks WM_HOTKEY; this model does not depend on that.
//! \[P] P9d/e must establish flag spoofing, IME/dead keys, secure-desktop visibility,
//! coherent physical button/drag snapshots, coordinate/delta meaning, and clock conversion.
//! The adapter must enforce native admission, prompt callback return and capture teardown.
//! Unknown keys are swallowed during capture but never translated through virtual-key guesses.

use std::collections::{BTreeMap, BTreeSet};

use crosspane_input::Held;
use crosspane_platform::{
    CaptureEvent, CaptureId, CapturePortal, CaptureStart, Chord, Edge, EndReason, HotkeyEvent,
    MotionKind, PlatformError, PortalId,
};
use crosspane_types::geom::PixelRect;
use crosspane_types::hid::{
    HidUsage, MouseButton, ScanPrefix, WinScancode, hid_to_windows, windows_to_hid,
};
use crosspane_types::id::DisplayId;
use crosspane_types::input::{LockKeys, ScrollDelta, ScrollPhase};
use crosspane_types::time::MonoTime;

/// \[E] <https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-kbdllhookstruct>
pub const LLKHF_EXTENDED: u32 = 0x01;
/// \[E] <https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-kbdllhookstruct>
pub const LLKHF_LOWER_IL_INJECTED: u32 = 0x02;
/// \[E] <https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-kbdllhookstruct>
pub const LLKHF_INJECTED: u32 = 0x10;
/// \[E] <https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-kbdllhookstruct>
pub const LLKHF_ALTDOWN: u32 = 0x20;
/// \[E] <https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-kbdllhookstruct>
pub const LLKHF_UP: u32 = 0x80;
/// \[E] <https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-msllhookstruct>
pub const LLMHF_INJECTED: u32 = 0x01;
/// \[E] <https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-msllhookstruct>
pub const LLMHF_LOWER_IL_INJECTED: u32 = 0x02;
/// \[E] <https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-msllhookstruct>
pub const WHEEL_DELTA: i32 = 120;
/// \[E] <https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-msllhookstruct>
pub const XBUTTON1: u32 = 1;
/// \[E] <https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-msllhookstruct>
pub const XBUTTON2: u32 = 2;
/// \[E] <https://learn.microsoft.com/en-us/windows/win32/winmsg/lowlevelkeyboardproc>
pub const LOW_LEVEL_HOOKS_TIMEOUT_MAX_MS: u64 = 1000;

/// \[P] The adapter supplies a monotonic receipt time and decoded flags; vk is never a fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyIn {
    pub scancode: u8,
    pub extended: bool,
    pub vk: u32,
    pub up: bool,
    pub injected: bool,
    pub ours: bool,
    pub at: MonoTime,
}

/// Literal (scan, extended, vk) snapshot; vk distinguishes zero-scan keys only.
/// \[U] Distinct zero-scan keys still have no HID usage; vk is never a translation fallback.
pub type KeySnapshot = (u8, bool, u32);

/// \[E] pt is per-monitor-aware screen coordinates; \[P] the adapter normalizes physical units.
/// Normal motion attests no drag; an optional snapshot reconciles the eight native button slots.
/// \[P] delta is physical motion before clamping; None derives only observable point changes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MouseIn {
    pub kind: MouseKind,
    pub pt: (i32, i32),
    pub delta: Option<(f64, f64)>,
    pub dragged: bool,
    pub buttons_down: Option<[bool; 8]>,
    pub injected: bool,
    pub ours: bool,
    pub at: MonoTime,
}

/// Model button numbers are zero-based: left/right/middle/back/forward; no native constants copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseKind {
    Move,
    Button { n: u8, down: bool },
    Wheel { v120: i32, horizontal: bool },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Decision {
    pub suppress: bool,
    pub events: Vec<CaptureEvent>,
}

/// Native dispatch facts, never inferred from a generic submission error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Native owner consumes these facts; pure tests compile this exact source.
pub(crate) enum DragSettlementResult {
    Accepted,
    KnownZero,
    Uncertain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Native drag configuration is cfg-gated; the model remains OS-free.
#[repr(u8)]
enum DragPhase {
    Reserved = 1,
    Dispatching = 2,
    InjectedFirst = 3,
    PhysicalFirst = 4,
    Uncertain = 5,
}

/// The ONE primary token's event-order provenance. The LL callback uses this same O(1) table.
/// The first local UP passes; only its matching counterpart is swallowed. SendInput counts
/// cannot prove which event was delivered. Zero is idle, not a separate button ledger.
#[allow(dead_code)]
pub(crate) fn drag_up_transition(order: u8, tagged: bool) -> (u8, bool) {
    match (order, tagged) {
        (2 | 5, true) => (3, false),
        (4, true) => (0, true),
        (1, false) => (0, false),
        (2 | 5, false) => (4, false),
        (3, false) => (0, true),
        _ => (order, false),
    }
}
impl DragPhase {
    fn from_order(order: u8) -> Option<Self> {
        match order {
            1 => Some(Self::Reserved),
            2 => Some(Self::Dispatching),
            3 => Some(Self::InjectedFirst),
            4 => Some(Self::PhysicalFirst),
            5 => Some(Self::Uncertain),
            _ => None,
        }
    }
}

/// The supplied portal rectangles are complete global physical strips, not logical rectangles.
/// CaptureId uniqueness is the frozen engine contract; old suppression tokens survive end.
#[derive(Debug)]
pub struct HookState {
    portals: BTreeMap<PortalId, (CapturePortal, PixelRect)>,
    pressed: BTreeSet<PortalId>,
    capture: Option<(CaptureId, DisplayId)>,
    local_keys: BTreeSet<KeySnapshot>,
    suppressed_keys: BTreeMap<KeySnapshot, CaptureId>,
    suppressed_buttons: [Option<CaptureId>; 256],
    buttons: [bool; 256],
    point: Option<(i32, i32)>,
    // Provenance for the existing suppressed-primary token, not another button ledger.
    drag_phase: Option<DragPhase>,
    drag_nonce: usize,
    drag_report: Option<DragSettlementResult>,
}

impl Default for HookState {
    fn default() -> Self {
        Self::new()
    }
}

impl HookState {
    pub fn new() -> Self {
        Self {
            portals: BTreeMap::new(),
            pressed: BTreeSet::new(),
            capture: None,
            local_keys: BTreeSet::new(),
            suppressed_keys: BTreeMap::new(),
            suppressed_buttons: [None; 256],
            buttons: [false; 256],
            point: None,
            drag_phase: None,
            drag_nonce: 0,
            drag_report: None,
        }
    }

    pub fn set_portals(
        &mut self,
        portals: Vec<(CapturePortal, PixelRect)>,
        at: MonoTime,
    ) -> Result<Vec<CaptureEvent>, PlatformError> {
        let mut next = BTreeMap::new();
        for (portal, rect) in portals {
            if !portal.from.is_finite()
                || !portal.to.is_finite()
                || portal.from < 0.0
                || portal.from >= portal.to
                || rect.min.x >= rect.max.x
                || rect.min.y >= rect.max.y
                || next.insert(portal.id, (portal, rect)).is_some()
            {
                return Err(PlatformError::Backend("invalid physical portal".into()));
            }
        }
        let mut events = Vec::new();
        self.pressed.retain(|id| {
            let keep = self.portals.get(id) == next.get(id);
            if !keep {
                events.push(CaptureEvent::EdgeReleased { portal: *id, at });
            }
            keep
        });
        self.portals = next;
        Ok(events)
    }

    /// Pure activation, not native readiness. Fresh snapshots prevent locally held ups being lost.
    /// The returned events precede subsequent input; lock state is supplied literally, never guessed.
    pub fn begin(
        &mut self,
        id: CaptureId,
        portal: PortalId,
        held: &[KeySnapshot],
        buttons: &[bool; 256],
        lock_keys: LockKeys,
        now: MonoTime,
    ) -> Result<(CaptureStart, Vec<CaptureEvent>), PlatformError> {
        self.begin_inner(id, portal, held, buttons, lock_keys, now, false)
    }

    #[allow(clippy::too_many_arguments)] // Same frozen activation facts plus the private mode.
    fn begin_inner(
        &mut self,
        id: CaptureId,
        portal: PortalId,
        held: &[KeySnapshot],
        buttons: &[bool; 256],
        lock_keys: LockKeys,
        now: MonoTime,
        drag: bool,
    ) -> Result<(CaptureStart, Vec<CaptureEvent>), PlatformError> {
        if self.capture.is_some() {
            return Err(PlatformError::Backend("capture already active".into()));
        }
        if buttons.iter().any(|down| *down) || (!drag && self.drag_phase.is_some()) {
            return Err(PlatformError::PointerButtonHeld);
        }
        let display = self
            .portals
            .get(&portal)
            .ok_or(PlatformError::NotFound)?
            .0
            .display;
        let keys: BTreeSet<_> = held.iter().copied().map(key_identity).collect();
        let usages: BTreeSet<_> = keys
            .iter()
            .filter_map(|&(code, extended, _)| key_usage(code, extended))
            .collect();
        self.suppressed_keys.retain(|key, _| keys.contains(key));
        self.local_keys = keys
            .into_iter()
            .filter(|key| !self.suppressed_keys.contains_key(key))
            .collect();
        self.buttons = *buttons;
        let mut events = self.release_edges(now);
        self.capture = Some((id, display));
        events.push(CaptureEvent::Started { id });
        Ok((
            CaptureStart {
                held_keys: usages.into_iter().collect(),
                lock_keys,
            },
            events,
        ))
    }

    pub fn end(&mut self, reason: EndReason, _now: MonoTime) -> Vec<CaptureEvent> {
        self.capture
            .take()
            .map_or_else(Vec::new, |(id, _)| vec![CaptureEvent::Ended { id, reason }])
    }

    /// Native activation consumes the entering portal rather than cancelling its pending crossing.
    /// Existing pure-model callers retain their original release notification behaviour.
    pub fn native_begin(
        &mut self,
        id: CaptureId,
        portal: PortalId,
        held: &[KeySnapshot],
        buttons: &[bool; 256],
        lock_keys: LockKeys,
        now: MonoTime,
    ) -> Result<(CaptureStart, Vec<CaptureEvent>), PlatformError> {
        let (start, mut events) = self.begin(id, portal, held, buttons, lock_keys, now)?;
        events.retain(|event| !matches!(event, CaptureEvent::EdgeReleased { portal: consumed, .. } if *consumed == portal));
        Ok((start, events))
    }

    /// Reserve the one locally owned physical down BEFORE attempting its synthetic up.
    #[allow(dead_code)] // Native owner and source-included pure contract tests only.
    pub(crate) fn protect_drag_tail(
        &mut self,
        id: CaptureId,
        button: MouseButton,
        nonce: usize,
    ) -> Result<(), PlatformError> {
        if button != MouseButton::PRIMARY
            || nonce == 0
            || self.capture.is_some()
            || !self.buttons[0]
            || self.buttons[1..].iter().any(|held| *held)
            || self.suppressed_buttons[0].is_some()
            || self.drag_phase.is_some()
        {
            return Err(PlatformError::PointerButtonHeld);
        }
        self.suppressed_buttons[0] = Some(id);
        self.drag_phase = Some(DragPhase::Reserved);
        self.drag_nonce = nonce;
        self.drag_report = None;
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn mark_drag_dispatch(&mut self, id: CaptureId) -> Result<(), PlatformError> {
        if self.suppressed_buttons[0] != Some(id) || self.drag_phase != Some(DragPhase::Reserved) {
            return Err(PlatformError::Backend(
                "drag reservation unavailable".into(),
            ));
        }
        self.drag_phase = Some(DragPhase::Dispatching);
        Ok(())
    }

    /// A send-count report is never settlement proof. Zero with no tagged UP rolls back an
    /// intact reservation, or completes PhysicalFirst without waiting for an impossible tag.
    #[allow(dead_code)]
    pub(crate) fn record_drag_settlement(
        &mut self,
        id: CaptureId,
        result: DragSettlementResult,
    ) -> Result<(), PlatformError> {
        if self.suppressed_buttons[0] != Some(id)
            || self.drag_report.is_some()
            || !matches!(
                self.drag_phase,
                Some(DragPhase::Dispatching | DragPhase::InjectedFirst | DragPhase::PhysicalFirst)
            )
        {
            return Err(PlatformError::Backend("drag dispatch unavailable".into()));
        }
        self.drag_report = Some(result);
        match result {
            DragSettlementResult::Accepted => {}
            DragSettlementResult::Uncertain if self.drag_phase == Some(DragPhase::Dispatching) => {
                self.drag_phase = Some(DragPhase::Uncertain)
            }
            DragSettlementResult::KnownZero
                if self.drag_phase != Some(DragPhase::InjectedFirst) =>
            {
                self.suppressed_buttons[0] = None;
                self.drag_phase = None;
                self.drag_nonce = 0;
            }
            _ => {}
        }
        Ok(())
    }

    /// Only the capture-owned unique nonce, never the ordinary injector tag, admits this UP.
    #[allow(dead_code)]
    pub(crate) fn drag_tagged_up(&mut self, nonce: usize) -> Decision {
        let mut decision = Decision {
            suppress: false,
            events: Vec::new(),
        };
        if nonce == 0 || self.drag_nonce != nonce {
            return decision;
        }
        let (order, suppress) = drag_up_transition(self.drag_phase.map_or(0, |p| p as u8), true);
        decision.suppress = suppress;
        self.drag_phase = DragPhase::from_order(order);
        if order == 0 {
            self.suppressed_buttons[0] = None;
            self.drag_nonce = 0;
        }
        decision
    }

    /// Cancellation before any dispatch; this cannot release an accepted/uncertain up's tail.
    #[allow(dead_code)]
    pub(crate) fn cancel_unsubmitted_drag(&mut self, id: CaptureId) -> Result<(), PlatformError> {
        if self.suppressed_buttons[0] != Some(id) || self.drag_phase != Some(DragPhase::Reserved) {
            return Err(PlatformError::Backend("drag already dispatched".into()));
        }
        self.suppressed_buttons[0] = None;
        self.drag_phase = None;
        self.drag_nonce = 0;
        Ok(())
    }

    /// Accepted is report consistency only; InjectedFirst is the observed-UP proof. Caller
    /// separately admits the original identity and matching actual MOVESIZEEND.
    #[allow(dead_code, clippy::too_many_arguments)] // Frozen activation facts plus primary button.
    pub(crate) fn native_begin_drag(
        &mut self,
        id: CaptureId,
        portal: PortalId,
        button: MouseButton,
        held: &[KeySnapshot],
        buttons: &[bool; 256],
        locks: LockKeys,
        at: MonoTime,
    ) -> Result<(CaptureStart, Vec<CaptureEvent>), PlatformError> {
        if button != MouseButton::PRIMARY
            || self.drag_phase != Some(DragPhase::InjectedFirst)
            || self.drag_report != Some(DragSettlementResult::Accepted)
            || self.suppressed_buttons[0] != Some(id)
            || !self.buttons[0]
            || !buttons[0]
            || buttons[1..].iter().any(|held| *held)
        {
            return Err(PlatformError::PointerButtonHeld);
        }
        let mut settled = *buttons;
        settled[0] = false;
        let (start, mut events) = self.begin_inner(id, portal, held, &settled, locks, at, true)?;
        self.buttons[0] = true;
        events.retain(|event| !matches!(event, CaptureEvent::EdgeReleased { portal: consumed, .. } if *consumed == portal));
        Ok((start, events))
    }

    #[allow(dead_code)]
    pub(crate) fn drag_tail_pending(&self) -> bool {
        self.drag_phase
            .is_some_and(|p| p != DragPhase::PhysicalFirst)
            && self.suppressed_buttons[0].is_some()
    }

    fn release_edges(&mut self, at: MonoTime) -> Vec<CaptureEvent> {
        std::mem::take(&mut self.pressed)
            .into_iter()
            .map(|portal| CaptureEvent::EdgeReleased { portal, at })
            .collect()
    }

    pub fn on_key(&mut self, key: KeyIn) -> Decision {
        if key.injected || key.ours {
            return Decision::default();
        }
        let i = key_identity((key.scancode, key.extended, key.vk));
        let usage = key_usage(key.scancode, key.extended);
        let mut decision = Decision {
            suppress: self.capture.is_some(),
            events: Vec::new(),
        };
        let report = if self.local_keys.contains(&i) {
            if key.up {
                self.local_keys.remove(&i);
                decision.suppress = false;
            }
            key.up
        } else if self.suppressed_keys.contains_key(&i) {
            decision.suppress = true;
            if key.up {
                self.suppressed_keys.remove(&i);
            }
            key.up
        } else {
            if let Some((id, _)) = self.capture
                && !key.up
            {
                self.suppressed_keys.insert(i, id);
            }
            !key.up
        };
        if self.capture.is_some()
            && report
            && let Some(usage) = usage
        {
            decision.events.push(CaptureEvent::Key {
                usage,
                down: !key.up,
                at: key.at,
            });
        }
        decision
    }

    /// Native queue records retain their capture generation. Old swallowed tails update the
    /// ledger without becoming events of a new capture; existing model callers are unchanged.
    pub fn queued_key(&mut self, observed: Option<CaptureId>, key: KeyIn) -> Decision {
        let old_tail = self
            .suppressed_keys
            .get(&key_identity((key.scancode, key.extended, key.vk)))
            .is_some_and(|token| Some(*token) != self.capture.map(|(id, _)| id));
        let mut decision = self.on_key(key);
        if old_tail || observed != self.capture.map(|(id, _)| id) {
            decision.events.clear();
        }
        decision
    }

    pub fn on_mouse(&mut self, mouse: MouseIn) -> Decision {
        if mouse.injected || mouse.ours {
            return Decision::default();
        }
        let previous = self.point.replace(mouse.pt);
        let delta = mouse
            .delta
            .or_else(|| {
                previous.map(|(x, y)| {
                    (
                        f64::from(mouse.pt.0) - f64::from(x),
                        f64::from(mouse.pt.1) - f64::from(y),
                    )
                })
            })
            .filter(|(dx, dy)| dx.is_finite() && dy.is_finite());
        let mut decision = Decision {
            suppress: self.capture.is_some(),
            events: Vec::new(),
        };
        match mouse.kind {
            MouseKind::Button { n, down } => {
                let i = usize::from(n);
                self.buttons[i] = down;
                // The original physical UP already passed. Only its tagged counterpart is
                // still ours; a later local down must not borrow that suppression token.
                if i == 0 && down && self.drag_phase == Some(DragPhase::PhysicalFirst) {
                    decision.suppress = false;
                    return decision;
                }
                if i == 0 && !down && self.drag_phase.is_some() {
                    let (order, suppress) =
                        drag_up_transition(self.drag_phase.map_or(0, |p| p as u8), false);
                    if !suppress {
                        self.drag_phase = DragPhase::from_order(order);
                        if order == 0 {
                            self.suppressed_buttons[0] = None;
                            self.drag_nonce = 0;
                        }
                        decision.suppress = false;
                        return decision;
                    }
                }
                if down {
                    decision.events.extend(self.release_edges(mouse.at));
                }
                let report = if let Some(token) = self.suppressed_buttons[i] {
                    decision.suppress = true;
                    if !down {
                        self.suppressed_buttons[i] = None;
                        if i == 0 {
                            self.drag_phase = None;
                            self.drag_nonce = 0;
                        }
                    }
                    !down && self.capture.is_some_and(|(id, _)| id == token)
                } else if let Some((id, _)) = self.capture {
                    if down {
                        self.suppressed_buttons[i] = Some(id);
                    }
                    down
                } else {
                    false
                };
                if report && let Some(button) = n.checked_add(1).map(MouseButton) {
                    decision.events.push(CaptureEvent::Button {
                        button,
                        down,
                        at: mouse.at,
                    });
                }
            }
            MouseKind::Wheel { v120, horizontal } => {
                if self.capture.is_some() {
                    decision.events.push(CaptureEvent::Scroll {
                        delta: ScrollDelta {
                            v120_x: if horizontal { v120 } else { 0 },
                            v120_y: if horizontal { 0 } else { v120 },
                            pixels: None,
                            phase: ScrollPhase::Discrete,
                            stop_x: false,
                            stop_y: false,
                        },
                        at: mouse.at,
                    });
                }
            }
            MouseKind::Move => {
                if let Some((_, display)) = self.capture {
                    if let Some((dx, dy)) = delta {
                        decision.events.push(CaptureEvent::Motion {
                            dx,
                            dy,
                            kind: MotionKind::Accelerated { display },
                            at: mouse.at,
                        });
                    }
                } else {
                    if let Some(snapshot) = mouse.buttons_down {
                        self.buttons[..8].copy_from_slice(&snapshot);
                    }
                    if mouse.dragged || self.buttons.iter().any(|held| *held) {
                        decision.events.extend(self.release_edges(mouse.at));
                    } else {
                        let hits: BTreeMap<_, _> = self
                            .portals
                            .iter()
                            .filter_map(|(id, (p, rect))| {
                                let (x, y) = mouse.pt;
                                let outward = delta.is_some_and(|(dx, dy)| match p.edge {
                                    Edge::Left => dx < 0.0,
                                    Edge::Right => dx > 0.0,
                                    Edge::Top => dy < 0.0,
                                    Edge::Bottom => dy > 0.0,
                                });
                                if !outward
                                    || x < rect.min.x
                                    || x >= rect.max.x
                                    || y < rect.min.y
                                    || y >= rect.max.y
                                {
                                    return None;
                                }
                                let (along, from, to) = match p.edge {
                                    Edge::Left | Edge::Right => (y, rect.min.y, rect.max.y),
                                    Edge::Top | Edge::Bottom => (x, rect.min.x, rect.max.x),
                                };
                                Some((
                                    *id,
                                    (f64::from(along) - f64::from(from))
                                        / (f64::from(to) - f64::from(from)),
                                ))
                            })
                            .collect();
                        for portal in self.pressed.difference(&hits.keys().copied().collect()) {
                            decision.events.push(CaptureEvent::EdgeReleased {
                                portal: *portal,
                                at: mouse.at,
                            });
                        }
                        for (&portal, &position) in &hits {
                            decision.events.push(CaptureEvent::EdgePressed {
                                portal,
                                position,
                                at: mouse.at,
                            });
                        }
                        self.pressed = hits.into_keys().collect();
                    }
                }
            }
        }
        decision
    }

    /// \[P] Only after capture is ended and native keyboard visibility has returned.
    /// Literal snapshots reconcile missed tails; they are not generated or refreshed by this model.
    pub fn reconcile(&mut self, key_down: &[KeySnapshot], buttons: &[bool; 256]) {
        if self.capture.is_some() {
            return;
        }
        let keys: BTreeSet<_> = key_down.iter().copied().map(key_identity).collect();
        self.suppressed_keys.retain(|key, _| keys.contains(key));
        self.local_keys = keys
            .into_iter()
            .filter(|key| !self.suppressed_keys.contains_key(key))
            .collect();
        let protected_primary = self.buttons[0];
        self.buttons = *buttons;
        if self.drag_phase.is_some() {
            self.buttons[0] = protected_primary;
        }
        for (i, &down) in buttons.iter().enumerate() {
            if !down && !(i == 0 && self.drag_phase.is_some()) {
                self.suppressed_buttons[i] = None;
            }
        }
    }

    /// Safe model stop condition: no active capture and no swallowed physical tails outstanding.
    pub fn all_released(&self) -> bool {
        self.capture.is_none()
            && self.suppressed_keys.is_empty()
            && self.suppressed_buttons.iter().all(Option::is_none)
    }
}

fn key_identity((scancode, extended, vk): KeySnapshot) -> KeySnapshot {
    (scancode, extended, if scancode == 0 { vk } else { 0 })
}

/// Reuse the frozen table verbatim, including its Pause/NumLock/PrintScreen quirks.
pub fn key_usage(scancode: u8, extended: bool) -> Option<HidUsage> {
    windows_to_hid(WinScancode {
        code: scancode,
        prefix: if extended {
            ScanPrefix::E0
        } else {
            ScanPrefix::None
        },
    })
}

/// Physical release authority independent of capture. Repeats and injected events are inert.
#[derive(Debug, Default)]
pub struct ChordTracker {
    chord: Option<Chord>,
    held: BTreeSet<HidUsage>,
    pressed: bool,
    armed: bool,
}

impl ChordTracker {
    /// Replacing a chord settles the old observation and reports an already-held new chord.
    pub fn set_chord(&mut self, chord: &Chord, at: MonoTime) -> Vec<HotkeyEvent> {
        if self.chord.as_ref() == Some(chord) {
            return Vec::new();
        }
        let mut events = Vec::new();
        if self.pressed {
            events.push(HotkeyEvent::Released { at });
        }
        self.chord = Some(chord.clone());
        self.pressed = self.matches();
        self.armed = !self.held.contains(&chord.key);
        if self.pressed {
            events.push(HotkeyEvent::Pressed { at });
        }
        events
    }

    fn matches(&self) -> bool {
        self.chord.as_ref().is_some_and(|chord| {
            self.held.contains(&chord.key) && chord.modifiers.iter().all(|m| self.held.contains(m))
        })
    }

    pub fn on_key(
        &mut self,
        usage: HidUsage,
        down: bool,
        injected: bool,
        at: MonoTime,
    ) -> Option<HotkeyEvent> {
        let changed = if injected {
            return None;
        } else if down {
            self.held.insert(usage)
        } else {
            self.held.remove(&usage)
        };
        if !changed {
            return None;
        }
        if self
            .chord
            .as_ref()
            .is_some_and(|chord| usage == chord.key && !down)
        {
            self.armed = true;
        }
        if self.pressed && !self.matches() {
            self.pressed = false;
            return Some(HotkeyEvent::Released { at });
        }
        if down
            && self.armed
            && self.matches()
            && self.chord.as_ref().is_some_and(|chord| usage == chord.key)
        {
            self.pressed = true;
            self.armed = false;
            return Some(HotkeyEvent::Pressed { at });
        }
        None
    }
}

/// An injector plan, never a native SendInput result. ScanPrefix is retained rather than losing E1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputRecord {
    Key { scancode: WinScancode, up: bool },
    MouseUp { button: MouseButton },
}

/// All-or-nothing translation; unsupported journal entries must not claim a complete release.
pub fn release_plan(held: &[Held]) -> Result<Vec<InputRecord>, PlatformError> {
    held.iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|held| match held {
            Held::Key(usage) => hid_to_windows(usage)
                .map(|scancode| InputRecord::Key { scancode, up: true })
                .ok_or(PlatformError::Unsupported("unmapped recovery key")),
            Held::Button(button) if (1..=5).contains(&button.0) => {
                Ok(InputRecord::MouseUp { button })
            }
            Held::Button(_) => Err(PlatformError::Unsupported("unmapped recovery button")),
        })
        .collect()
}
