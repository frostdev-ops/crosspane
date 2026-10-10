//! The activation state machine of the InputCapture backend (pure: no I/O, no threads, time is an
//! argument).
//!
//! The compositor activates a sticky barrier on its own: from that moment it holds the pointer and
//! consumes every key, button, scroll and motion until the activation is released. This machine
//! turns that into the `InputCapture` contract (amendments A1-A3 of WP-G1.7):
//!
//! - **Pending (A1).** An activation that `begin` has not adopted routes nothing. Every event is
//!   consumed and only counted, except that motion drives a *virtual pointer* (its distance from
//!   the barrier and its place along the portal) which becomes `EdgePressed`, once per EIS frame
//!   that moved the pointer. The activation is released to its origin point (nudged two logical
//!   pixels inward) when the virtual pointer moves more than [`INWARD_BOX`] logical pixels away
//!   from the barrier, leaves the portal's stretch, the gate closes, the portal is removed, or
//!   [`PENDING_MAX`] passes without a `begin`. Each release after an `EdgePressed` is followed by
//!   `EdgeReleased`.
//! - **Active (A2).** `begin` adopts the pending activation synchronously: `Started` precedes every
//!   other capture event, `held_keys` are the keys pressed since the activation (A3), and a button
//!   pressed since the activation refuses the begin (`PointerButtonHeld`). Every failed `begin`
//!   releases the activation. `end` releases it to the point the caller computed.
//! - **Loss.** An activation the compositor ended on its own (the escape chord, a zone change, the
//!   session disabled) is `Ended { Lost }` for an active capture and `EdgeReleased` for a pending
//!   one, and asks to re-arm the portal session ([`Out::Rearm`]). An activation this machine
//!   released itself is expected and silent.
//!
//! **Which events belong to an activation.** Every EIS device announces `start_emulating` with a
//! sequence number, which mutter sets to the activation id. The receiver tags each input with its
//! device's sequence. Input for an activation the `Activated` signal has not announced yet (the EIS
//! socket can be faster than the D-Bus signal) is held back and replayed when the signal arrives;
//! input for an activation that is over is dropped.
//!
//! Logs and counters never record key or button codes.

use std::collections::BTreeSet;
use std::time::Duration;

use crosspane_platform::{
    CaptureEvent, CaptureId, CaptureStart, Edge, EndReason, MotionKind, PlatformError, PortalId,
};
use crosspane_types::geom::VectorLogical;
use crosspane_types::hid::{HidUsage, MouseButton, evdev_to_hid};
use crosspane_types::input::{LockKeys, ScrollDelta, ScrollPhase};
use crosspane_types::time::MonoTime;

use super::barriers::Entry;

/// A pending activation nobody adopted is released after this long: push-to-cross (default 0) plus
/// the capture indicator (500 ms) plus the handshake (1 s) plus margin.
pub(super) const PENDING_MAX: Duration = Duration::from_secs(3);
/// The virtual pointer may move this far (logical px) away from the barrier before the pending
/// activation counts as abandoned: mutter's own barrier hit box.
pub(super) const INWARD_BOX: f64 = 2.0;
/// A released activation puts the pointer this far (logical px) inside its origin, so that the
/// compositor's truncating warp lands inside the display and not on the barrier it just left
/// (mutter reported an origin x of -6e-8 on a left edge).
const NUDGE: f64 = 2.0;
/// Input held back for an activation whose signal has not arrived. Far above what one activation
/// can produce before its signal; the excess is dropped.
const EARLY_MAX: usize = 1024;
/// Activations this machine released whose `Deactivated` is still expected.
const RELEASED_KEEP: usize = 8;

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;

/// One scroll event of the EIS receiver (EIS convention: positive y scrolls down).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Scroll {
    Smooth {
        dx: f64,
        dy: f64,
    },
    /// 120ths of a wheel detent.
    Discrete {
        dx: i32,
        dy: i32,
    },
    Stop {
        x: bool,
        y: bool,
    },
    Cancel {
        x: bool,
        y: bool,
    },
}

/// One input of the EIS receiver, in logical pixels and evdev codes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Input {
    Motion {
        dx: f64,
        dy: f64,
    },
    Key {
        code: u16,
        down: bool,
    },
    Button {
        code: u32,
        down: bool,
    },
    Scroll(Scroll),
    /// The device closed a group of events.
    Frame,
}

/// What the machine asks its owner to do, in order.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Out {
    /// Deliver to the subscriber.
    Emit(CaptureEvent),
    /// `Release(activation, cursor_position)` on the portal.
    Release { activation: u32, at: (f64, f64) },
    /// The compositor ended the activation or the session on its own: Disable, GetZones,
    /// SetPointerBarriers, Enable again.
    Rearm,
}

/// The result of `begin`: what to do either way, and the verdict.
#[derive(Debug)]
pub(super) struct Begin {
    pub outs: Vec<Out>,
    pub result: Result<CaptureStart, PlatformError>,
}

/// Keys and buttons that went down since the activation.
#[derive(Debug, Default)]
struct Ledger {
    keys: BTreeSet<u16>,
    buttons: BTreeSet<u32>,
}

impl Ledger {
    /// Record a key change; `true` for a new down. An up is `true` when the key was down.
    fn key(&mut self, code: u16, down: bool) -> bool {
        if down {
            self.keys.insert(code)
        } else {
            self.keys.remove(&code)
        }
    }

    fn button(&mut self, code: u32, down: bool) -> bool {
        if down {
            self.buttons.insert(code)
        } else {
            self.buttons.remove(&code)
        }
    }

    /// The held keys that have a HID usage, in a stable order.
    fn held_usages(&self) -> Vec<HidUsage> {
        let mut usages: Vec<HidUsage> = self.keys.iter().filter_map(|c| evdev_to_hid(*c)).collect();
        usages.sort();
        usages
    }
}

/// How many events a pending activation consumed (counts only, for the log).
#[derive(Clone, Copy, Debug, Default)]
struct Counts {
    motions: u32,
    keys: u32,
    buttons: u32,
    scrolls: u32,
}

#[derive(Debug)]
struct Pending {
    entry: Entry,
    activation: u32,
    origin: (f64, f64),
    /// The virtual pointer along the barrier, absolute logical coordinate.
    along: f64,
    /// The virtual pointer's distance from the barrier along the outward normal: 0 at the barrier
    /// (pushing outwards does not go beyond it), negative inside.
    normal: f64,
    since: MonoTime,
    ledger: Ledger,
    counts: Counts,
    /// Motion since the last frame.
    dirty: bool,
    /// An `EdgePressed` was reported and no `EdgeReleased` since.
    pressing: bool,
}

#[derive(Debug)]
struct Active {
    entry: Entry,
    activation: u32,
    id: CaptureId,
    origin: (f64, f64),
    ledger: Ledger,
    /// Smooth scrolling in progress on (x, y).
    scrolling: (bool, bool),
}

#[derive(Debug, Default)]
enum Phase {
    #[default]
    Idle,
    Pending(Pending),
    Active(Active),
}

/// The state machine. See the module documentation.
#[derive(Debug, Default)]
pub(super) struct Machine {
    phase: Phase,
    /// The highest activation id the `Activated` signal announced.
    newest: u32,
    /// Activations released by this machine whose `Deactivated` has not come yet.
    released: Vec<u32>,
    /// Input for activations the signal has not announced yet.
    early: Vec<(u32, Input)>,
    /// Input dropped as stale or unattributable (a count, for the log).
    dropped: u64,
}

fn outward(edge: Edge) -> (f64, f64) {
    match edge {
        Edge::Left => (-1.0, 0.0),
        Edge::Right => (1.0, 0.0),
        Edge::Top => (0.0, -1.0),
        Edge::Bottom => (0.0, 1.0),
    }
}

/// Whether the barrier runs vertically (Left and Right edges), so the stretch is along y.
fn vertical(edge: Edge) -> bool {
    matches!(edge, Edge::Left | Edge::Right)
}

/// The origin `NUDGE` logical pixels inward.
fn nudged(entry: &Entry, origin: (f64, f64)) -> (f64, f64) {
    let (ux, uy) = outward(entry.edge);
    (origin.0 - ux * NUDGE, origin.1 - uy * NUDGE)
}

fn position_of(entry: &Entry, along: f64) -> f64 {
    let (lo, hi) = entry.span;
    if hi > lo {
        ((along - lo) / (hi - lo)).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// An evdev button code as a Crosspane button.
fn button_of(code: u32) -> Option<MouseButton> {
    match code {
        BTN_LEFT => Some(MouseButton::PRIMARY),
        BTN_RIGHT => Some(MouseButton::SECONDARY),
        BTN_MIDDLE => Some(MouseButton::TERTIARY),
        BTN_SIDE => Some(MouseButton::BACK),
        BTN_EXTRA => Some(MouseButton::FORWARD),
        _ => None,
    }
}

impl Pending {
    fn new(
        entry: Entry,
        activation: u32,
        origin: (f64, f64),
        position: f64,
        now: MonoTime,
    ) -> Self {
        let (lo, hi) = entry.span;
        // Strictly inside the stretch, so the first frame does not read as "off the portal".
        let along = (lo + position.clamp(0.0, 1.0) * (hi - lo))
            .min(hi - 1e-6)
            .max(lo);
        Pending {
            entry,
            activation,
            origin,
            along,
            normal: 0.0,
            since: now,
            ledger: Ledger::default(),
            counts: Counts::default(),
            dirty: false,
            pressing: false,
        }
    }

    fn release_at(&self) -> (f64, f64) {
        nudged(&self.entry, self.origin)
    }

    fn press(&mut self, now: MonoTime) -> Out {
        self.pressing = true;
        Out::Emit(CaptureEvent::EdgePressed {
            portal: self.entry.portal,
            position: position_of(&self.entry, self.along),
            at: now,
        })
    }

    /// Give the activation up: `Release`, and `EdgeReleased` if a press was reported.
    fn abandon(&self, now: MonoTime, why: &'static str) -> Vec<Out> {
        tracing::debug!(
            why,
            activation = self.activation,
            motions = self.counts.motions,
            keys = self.counts.keys,
            buttons = self.counts.buttons,
            scrolls = self.counts.scrolls,
            "releasing a pending capture activation"
        );
        let mut outs = vec![Out::Release {
            activation: self.activation,
            at: self.release_at(),
        }];
        if self.pressing {
            outs.push(self.edge_released(now));
        }
        outs
    }

    fn edge_released(&self, now: MonoTime) -> Out {
        Out::Emit(CaptureEvent::EdgeReleased {
            portal: self.entry.portal,
            at: now,
        })
    }

    /// Consume one input (A1: nothing is routed).
    fn apply(&mut self, now: MonoTime, input: Input) -> Vec<Out> {
        match input {
            Input::Motion { dx, dy } => {
                if dx.is_finite() && dy.is_finite() {
                    let (ux, uy) = outward(self.entry.edge);
                    self.normal = (self.normal + dx * ux + dy * uy).min(0.0);
                    self.along += if vertical(self.entry.edge) { dy } else { dx };
                    self.dirty = true;
                    self.counts.motions = self.counts.motions.saturating_add(1);
                }
                Vec::new()
            }
            Input::Key { code, down } => {
                self.ledger.key(code, down);
                self.counts.keys = self.counts.keys.saturating_add(1);
                Vec::new()
            }
            Input::Button { code, down } => {
                self.ledger.button(code, down);
                self.counts.buttons = self.counts.buttons.saturating_add(1);
                Vec::new()
            }
            Input::Scroll(_) => {
                self.counts.scrolls = self.counts.scrolls.saturating_add(1);
                Vec::new()
            }
            Input::Frame => {
                if !std::mem::take(&mut self.dirty) {
                    return Vec::new();
                }
                let (lo, hi) = self.entry.span;
                if self.normal < -INWARD_BOX {
                    self.abandon(now, "moved inward")
                } else if self.along < lo || self.along >= hi {
                    self.abandon(now, "left the portal")
                } else {
                    vec![self.press(now)]
                }
            }
        }
    }
}

impl Active {
    fn release_at(&self) -> (f64, f64) {
        nudged(&self.entry, self.origin)
    }

    /// Route one input (A2: only after `Started`).
    fn apply(&mut self, now: MonoTime, input: Input) -> Vec<Out> {
        let event = match input {
            Input::Motion { dx, dy } => {
                if !dx.is_finite() || !dy.is_finite() {
                    return Vec::new();
                }
                CaptureEvent::Motion {
                    dx: dx * self.entry.scale,
                    dy: dy * self.entry.scale,
                    kind: MotionKind::Accelerated {
                        display: self.entry.display,
                    },
                    at: now,
                }
            }
            Input::Key { code, down } => {
                let changed = self.ledger.key(code, down);
                // A repeated down is not a new press; an up is always delivered (A3: the key may
                // have been pressed before the activation).
                if down && !changed {
                    return Vec::new();
                }
                match evdev_to_hid(code) {
                    Some(usage) => CaptureEvent::Key {
                        usage,
                        down,
                        at: now,
                    },
                    None => return Vec::new(),
                }
            }
            Input::Button { code, down } => {
                let changed = self.ledger.button(code, down);
                if down && !changed {
                    return Vec::new();
                }
                match button_of(code) {
                    Some(button) => CaptureEvent::Button {
                        button,
                        down,
                        at: now,
                    },
                    None => return Vec::new(),
                }
            }
            Input::Scroll(scroll) => CaptureEvent::Scroll {
                delta: self.scroll(scroll),
                at: now,
            },
            Input::Frame => return Vec::new(),
        };
        vec![Out::Emit(event)]
    }

    /// The Crosspane scroll for an EIS one: EIS and Wayland count positive y as down, Crosspane
    /// positive y as up; x keeps its sign, as the Hyprland capture reports it.
    fn scroll(&mut self, scroll: Scroll) -> ScrollDelta {
        let mut delta = ScrollDelta {
            v120_x: 0,
            v120_y: 0,
            pixels: None,
            phase: ScrollPhase::Discrete,
            stop_x: false,
            stop_y: false,
        };
        match scroll {
            Scroll::Discrete { dx, dy } => {
                delta.v120_x = dx;
                delta.v120_y = dy.saturating_neg();
            }
            Scroll::Smooth { dx, dy } => {
                let was_scrolling = self.scrolling.0 || self.scrolling.1;
                if dx != 0.0 {
                    self.scrolling.0 = true;
                }
                if dy != 0.0 {
                    self.scrolling.1 = true;
                }
                delta.pixels = Some(VectorLogical::new(dx, -dy));
                delta.phase = if was_scrolling {
                    ScrollPhase::Changed
                } else {
                    ScrollPhase::Began
                };
            }
            Scroll::Stop { x, y } => {
                if x {
                    self.scrolling.0 = false;
                }
                if y {
                    self.scrolling.1 = false;
                }
                delta.pixels = Some(VectorLogical::new(0.0, 0.0));
                delta.stop_x = x;
                delta.stop_y = y;
                delta.phase = if !self.scrolling.0 && !self.scrolling.1 {
                    ScrollPhase::Ended
                } else {
                    ScrollPhase::Changed
                };
            }
            Scroll::Cancel { x, y } => {
                self.scrolling = (false, false);
                delta.pixels = Some(VectorLogical::new(0.0, 0.0));
                delta.stop_x = x;
                delta.stop_y = y;
                delta.phase = ScrollPhase::Cancelled;
            }
        }
        delta
    }
}

impl Machine {
    pub(super) fn new() -> Machine {
        Machine::default()
    }

    pub(super) fn is_idle(&self) -> bool {
        matches!(self.phase, Phase::Idle)
    }

    pub(super) fn is_active(&self) -> bool {
        matches!(self.phase, Phase::Active(_))
    }

    /// The portal of the pending or active activation.
    #[cfg(test)]
    pub(super) fn portal(&self) -> Option<PortalId> {
        match &self.phase {
            Phase::Idle => None,
            Phase::Pending(p) => Some(p.entry.portal),
            Phase::Active(a) => Some(a.entry.portal),
        }
    }

    /// The pending or active activation and the point `abort` would release it to.
    pub(super) fn live(&self) -> Option<(u32, (f64, f64))> {
        match &self.phase {
            Phase::Idle => None,
            Phase::Pending(p) => Some((p.activation, p.release_at())),
            Phase::Active(a) => Some((a.activation, a.release_at())),
        }
    }

    /// Whether the gate must be looked at often (something is pending or active).
    pub(super) fn busy(&self) -> bool {
        !self.is_idle()
    }

    /// Input dropped as stale or unattributable so far.
    pub(super) fn dropped(&self) -> u64 {
        self.dropped
    }

    fn remember_release(&mut self, activation: u32) {
        self.released.push(activation);
        if self.released.len() > RELEASED_KEEP {
            self.released.remove(0);
        }
    }

    /// The `Activated` signal. `found` is the portal and the position along it the barrier plan
    /// found for the barrier id and cursor (`None`: the activation matches no portal). `accept` is
    /// false while the gate is closed or nobody listens: the activation is released at once.
    pub(super) fn activated(
        &mut self,
        now: MonoTime,
        activation: u32,
        found: Option<(Entry, f64)>,
        cursor: (f64, f64),
        accept: bool,
    ) -> Vec<Out> {
        let mut outs = Vec::new();
        // The compositor allows one activation at a time and orders its signals, so an earlier
        // one still held here missed its `Deactivated`: it is over.
        outs.extend(self.lost(now));
        self.newest = self.newest.max(activation);
        self.early.retain(|(seq, _)| *seq >= activation);
        let Some((entry, position)) = found.filter(|_| accept) else {
            let at = found.map_or(cursor, |(entry, _)| nudged(&entry, cursor));
            tracing::debug!(
                activation,
                matched = found.is_some(),
                accept,
                "releasing a capture activation at once"
            );
            self.remember_release(activation);
            self.early.retain(|(seq, _)| *seq != activation);
            outs.push(Out::Release { activation, at });
            return outs;
        };
        let mut pending = Pending::new(entry, activation, cursor, position, now);
        outs.push(pending.press(now));
        // Input that outran the signal.
        let early = std::mem::take(&mut self.early);
        let (mine, later): (Vec<_>, Vec<_>) =
            early.into_iter().partition(|(seq, _)| *seq == activation);
        self.early = later;
        self.phase = Phase::Pending(pending);
        for (_, input) in mine {
            outs.extend(self.route(now, input));
        }
        outs
    }

    /// Apply `input` to the current activation.
    fn route(&mut self, now: MonoTime, input: Input) -> Vec<Out> {
        let outs = match &mut self.phase {
            Phase::Pending(p) => p.apply(now, input),
            Phase::Active(a) => a.apply(now, input),
            Phase::Idle => return Vec::new(),
        };
        self.settle(outs)
    }

    /// A pending activation that released itself is over.
    fn settle(&mut self, outs: Vec<Out>) -> Vec<Out> {
        let released = outs.iter().find_map(|out| match out {
            Out::Release { activation, .. } => Some(*activation),
            _ => None,
        });
        if let Some(activation) = released {
            self.remember_release(activation);
            self.phase = Phase::Idle;
        }
        outs
    }

    /// One input of the EIS receiver, tagged with its device's emulation sequence.
    pub(super) fn input(&mut self, now: MonoTime, seq: Option<u32>, input: Input) -> Vec<Out> {
        let Some(seq) = seq else {
            self.dropped = self.dropped.saturating_add(1);
            return Vec::new();
        };
        let mine = match &self.phase {
            Phase::Pending(p) => p.activation == seq,
            Phase::Active(a) => a.activation == seq,
            Phase::Idle => false,
        };
        if mine {
            return self.route(now, input);
        }
        if seq > self.newest {
            if self.early.len() < EARLY_MAX {
                self.early.push((seq, input));
            } else {
                self.dropped = self.dropped.saturating_add(1);
            }
        } else {
            self.dropped = self.dropped.saturating_add(1);
        }
        Vec::new()
    }

    /// The `Deactivated` signal.
    pub(super) fn deactivated(&mut self, now: MonoTime, activation: u32) -> Vec<Out> {
        if let Some(index) = self.released.iter().position(|id| *id == activation) {
            self.released.remove(index);
            return Vec::new();
        }
        let ours = match &self.phase {
            Phase::Pending(p) => p.activation == activation,
            Phase::Active(a) => a.activation == activation,
            Phase::Idle => false,
        };
        if !ours {
            return Vec::new();
        }
        tracing::info!(
            activation,
            "the compositor ended a capture activation (escape chord, layout change or disable)"
        );
        let mut outs = self.lost(now);
        outs.push(Out::Rearm);
        outs
    }

    /// The pending activation timed out, or the gate closed; checked every few milliseconds while
    /// [`busy`](Self::busy).
    pub(super) fn tick(&mut self, now: MonoTime, accept: bool) -> Vec<Out> {
        match &self.phase {
            Phase::Pending(p) => {
                let why = if !accept {
                    "gate closed"
                } else if now.saturating_duration_since(p.since) >= PENDING_MAX {
                    "no begin in time"
                } else {
                    return Vec::new();
                };
                let outs = p.abandon(now, why);
                self.settle(outs)
            }
            Phase::Active(a) if !accept => {
                let outs = self.leave_active(now, a.release_at(), EndReason::Lost);
                self.settle(outs)
            }
            _ => Vec::new(),
        }
    }

    /// `Release`, then `Ended { reason }` for the active capture, then `EdgeReleased`: the
    /// pointer is back inside the display, off the portal, so the engine may arm it again.
    fn leave_active(&self, now: MonoTime, at: (f64, f64), reason: EndReason) -> Vec<Out> {
        let Phase::Active(a) = &self.phase else {
            return Vec::new();
        };
        vec![
            Out::Release {
                activation: a.activation,
                at,
            },
            Out::Emit(CaptureEvent::Ended { id: a.id, reason }),
            Out::Emit(CaptureEvent::EdgeReleased {
                portal: a.entry.portal,
                at: now,
            }),
        ]
    }

    /// `InputCapture::begin`: adopt the pending activation.
    pub(super) fn begin(
        &mut self,
        now: MonoTime,
        id: CaptureId,
        portal: PortalId,
        accept: bool,
        lock_keys: LockKeys,
    ) -> Begin {
        let pending = match std::mem::take(&mut self.phase) {
            Phase::Pending(pending) => pending,
            other @ Phase::Active(_) => {
                self.phase = other;
                return Begin {
                    outs: Vec::new(),
                    result: Err(PlatformError::Backend("capture already active".into())),
                };
            }
            Phase::Idle => {
                // A closed gate is the more telling answer when the activation was already
                // released for it.
                return Begin {
                    outs: Vec::new(),
                    result: Err(if accept {
                        PlatformError::NotFound
                    } else {
                        PlatformError::Locked
                    }),
                };
            }
        };
        let refusal = if !accept {
            Some(PlatformError::Locked)
        } else if pending.entry.portal != portal {
            Some(PlatformError::NotFound)
        } else if !pending.ledger.buttons.is_empty() {
            Some(PlatformError::PointerButtonHeld)
        } else {
            None
        };
        if let Some(error) = refusal {
            // A2: a failed begin releases the activation, so nothing can activate later.
            let outs = pending.abandon(now, "begin refused");
            self.remember_release(pending.activation);
            return Begin {
                outs,
                result: Err(error),
            };
        }
        let start = CaptureStart {
            held_keys: pending.ledger.held_usages(),
            lock_keys,
        };
        tracing::debug!(
            activation = pending.activation,
            held = start.held_keys.len(),
            motions = pending.counts.motions,
            "adopted a pending capture activation"
        );
        self.phase = Phase::Active(Active {
            entry: pending.entry,
            activation: pending.activation,
            id,
            origin: pending.origin,
            ledger: Ledger {
                keys: pending.ledger.keys,
                buttons: BTreeSet::new(),
            },
            scrolling: (false, false),
        });
        Begin {
            outs: vec![Out::Emit(CaptureEvent::Started { id })],
            result: Ok(start),
        }
    }

    /// `InputCapture::end`: release the active capture to `at`. Nothing active is a no-op.
    pub(super) fn end(&mut self, now: MonoTime, at: (f64, f64)) -> Vec<Out> {
        if !self.is_active() {
            return Vec::new();
        }
        let outs = self.leave_active(now, at, EndReason::Requested);
        self.settle(outs)
    }

    /// The portal set changed: an activation on a portal that is gone or different is over.
    pub(super) fn portal_gone(
        &mut self,
        now: MonoTime,
        keep: impl Fn(PortalId) -> bool,
    ) -> Vec<Out> {
        match &self.phase {
            Phase::Pending(p) if !keep(p.entry.portal) => {
                let outs = p.abandon(now, "portal removed");
                self.settle(outs)
            }
            Phase::Active(a) if !keep(a.entry.portal) => {
                let outs = self.leave_active(now, a.release_at(), EndReason::Lost);
                self.settle(outs)
            }
            _ => Vec::new(),
        }
    }

    /// The session lost the activation without anything to release (closed, disabled, EIS gone).
    pub(super) fn lost(&mut self, now: MonoTime) -> Vec<Out> {
        match std::mem::take(&mut self.phase) {
            Phase::Idle => Vec::new(),
            Phase::Pending(p) => {
                if p.pressing {
                    vec![p.edge_released(now)]
                } else {
                    Vec::new()
                }
            }
            Phase::Active(a) => vec![
                Out::Emit(CaptureEvent::Ended {
                    id: a.id,
                    reason: EndReason::Lost,
                }),
                Out::Emit(CaptureEvent::EdgeReleased {
                    portal: a.entry.portal,
                    at: now,
                }),
            ],
        }
    }

    /// `CaptureAbort`: end everything now; `Ended { Aborted }` for an active capture.
    pub(super) fn abort(&mut self, now: MonoTime) -> Vec<Out> {
        let outs = match &self.phase {
            Phase::Idle => return Vec::new(),
            Phase::Pending(p) => p.abandon(now, "aborted"),
            Phase::Active(a) => self.leave_active(now, a.release_at(), EndReason::Aborted),
        };
        self.settle(outs)
    }

    /// Forget everything about the session's activations (a new session starts at 1 again).
    pub(super) fn reset(&mut self) {
        *self = Machine::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosspane_types::id::DisplayId;

    fn t(ms: u64) -> MonoTime {
        MonoTime::from_nanos(ms * 1_000_000)
    }

    /// HDMI-1's outer right stretch: x = 1080, y 600..1080, scale 1.
    fn right_entry() -> Entry {
        Entry {
            portal: PortalId(7),
            display: DisplayId(3),
            edge: Edge::Right,
            scale: 1.0,
            span: (600.0, 1080.0),
        }
    }

    fn top_entry() -> Entry {
        Entry {
            portal: PortalId(8),
            display: DisplayId(4),
            edge: Edge::Top,
            scale: 2.0,
            span: (100.0, 500.0),
        }
    }

    const LOCKS: LockKeys = LockKeys {
        caps_lock: Some(true),
        num_lock: None,
        scroll_lock: None,
    };

    fn pressed(outs: &[Out]) -> Vec<f64> {
        outs.iter()
            .filter_map(|out| match out {
                Out::Emit(CaptureEvent::EdgePressed { position, .. }) => Some(*position),
                _ => None,
            })
            .collect()
    }

    fn emits(outs: &[Out]) -> Vec<&CaptureEvent> {
        outs.iter()
            .filter_map(|out| match out {
                Out::Emit(event) => Some(event),
                _ => None,
            })
            .collect()
    }

    fn releases(outs: &[Out]) -> Vec<(u32, (f64, f64))> {
        outs.iter()
            .filter_map(|out| match out {
                Out::Release { activation, at } => Some((*activation, *at)),
                _ => None,
            })
            .collect()
    }

    /// An activation at the middle of the right stretch, pending.
    fn pending() -> (Machine, Vec<Out>) {
        let mut m = Machine::new();
        let outs = m.activated(t(0), 1, Some((right_entry(), 0.5)), (1080.0, 840.0), true);
        (m, outs)
    }

    fn motion(m: &mut Machine, ms: u64, dx: f64, dy: f64) -> Vec<Out> {
        let mut outs = m.input(t(ms), Some(1), Input::Motion { dx, dy });
        outs.extend(m.input(t(ms), Some(1), Input::Frame));
        outs
    }

    #[test]
    fn an_activation_reports_the_first_press_and_nothing_else() {
        let (m, outs) = pending();
        assert_eq!(pressed(&outs), vec![0.5]);
        assert_eq!(emits(&outs).len(), 1);
        assert!(releases(&outs).is_empty());
        assert_eq!(m.portal(), Some(PortalId(7)));
        assert!(m.busy() && !m.is_active());
        // Where abort would put the pointer: two pixels inside the barrier.
        assert_eq!(m.live(), Some((1, (1078.0, 840.0))));
    }

    #[test]
    fn pushing_repeats_the_press_once_per_frame_with_the_position_along_the_stretch() {
        let (mut m, _) = pending();
        // Pushing outwards keeps the virtual pointer on the barrier; sliding along moves it.
        let outs = motion(&mut m, 10, 30.0, 0.0);
        assert_eq!(pressed(&outs), vec![0.5]);
        let outs = motion(&mut m, 20, 5.0, 48.0);
        assert_eq!(pressed(&outs), vec![0.6]);
        // Two motions in one frame are one press.
        let mut outs = m.input(t(30), Some(1), Input::Motion { dx: 1.0, dy: 24.0 });
        outs.extend(m.input(t(30), Some(1), Input::Motion { dx: 1.0, dy: 24.0 }));
        assert!(outs.is_empty());
        let outs = m.input(t(30), Some(1), Input::Frame);
        assert_eq!(pressed(&outs), vec![0.7]);
        // A frame without motion presses nothing.
        assert!(m.input(t(40), Some(1), Input::Frame).is_empty());
    }

    #[test]
    fn a_pending_activation_swallows_keys_buttons_and_scrolls() {
        let (mut m, _) = pending();
        for input in [
            Input::Key {
                code: 30,
                down: true,
            },
            Input::Button {
                code: BTN_LEFT,
                down: true,
            },
            Input::Button {
                code: BTN_LEFT,
                down: false,
            },
            Input::Scroll(Scroll::Discrete { dx: 0, dy: 120 }),
            Input::Key {
                code: 30,
                down: false,
            },
        ] {
            assert!(m.input(t(5), Some(1), input).is_empty(), "{input:?}");
            assert!(m.input(t(5), Some(1), Input::Frame).is_empty());
        }
        assert!(m.busy() && !m.is_active());
    }

    #[test]
    fn moving_inward_more_than_the_hit_box_releases_to_the_origin_and_reports_it() {
        let (mut m, _) = pending();
        // 1.5 px inward: inside the box.
        assert_eq!(pressed(&motion(&mut m, 10, -1.5, 0.0)), vec![0.5]);
        // Pushing back out first only clamps at the barrier, so the next 2.5 px inward is net -2.5.
        let _ = motion(&mut m, 20, 40.0, 0.0);
        let outs = motion(&mut m, 30, -2.5, 0.0);
        assert_eq!(releases(&outs), vec![(1, (1078.0, 840.0))]);
        let events = emits(&outs);
        assert!(matches!(
            events.as_slice(),
            [CaptureEvent::EdgeReleased {
                portal: PortalId(7),
                ..
            }]
        ));
        assert!(m.is_idle());
        // The compositor's `Deactivated` for our own release is expected.
        assert!(m.deactivated(t(40), 1).is_empty());
    }

    #[test]
    fn leaving_the_stretch_releases_it() {
        let (mut m, _) = pending();
        // 240 px is the middle (840) to the end of the stretch (1080).
        let outs = motion(&mut m, 10, 0.0, 239.0);
        assert_eq!(pressed(&outs).len(), 1);
        let outs = motion(&mut m, 20, 0.0, 2.0);
        assert_eq!(releases(&outs).len(), 1);
        assert!(matches!(
            emits(&outs).as_slice(),
            [CaptureEvent::EdgeReleased { .. }]
        ));
        assert!(m.is_idle());
    }

    #[test]
    fn the_top_edge_slides_along_x_and_goes_inward_along_y() {
        let mut m = Machine::new();
        let _ = m.activated(t(0), 1, Some((top_entry(), 0.25)), (200.0, 0.0), true);
        // Top edge: outward is -y, so moving down is inward.
        let outs = m.input(
            t(5),
            Some(1),
            Input::Motion {
                dx: 100.0,
                dy: -9.0,
            },
        );
        let outs = [outs, m.input(t(5), Some(1), Input::Frame)].concat();
        assert_eq!(pressed(&outs), vec![0.5]);
        let mut outs = m.input(t(6), Some(1), Input::Motion { dx: 0.0, dy: 3.0 });
        outs.extend(m.input(t(6), Some(1), Input::Frame));
        assert_eq!(releases(&outs), vec![(1, (200.0, 2.0))]);
    }

    #[test]
    fn a_pending_activation_times_out_after_three_seconds() {
        let (mut m, _) = pending();
        assert!(m.tick(t(2_999), true).is_empty());
        let outs = m.tick(t(3_000), true);
        assert_eq!(releases(&outs), vec![(1, (1078.0, 840.0))]);
        assert!(matches!(
            emits(&outs).as_slice(),
            [CaptureEvent::EdgeReleased { .. }]
        ));
        assert!(m.is_idle() && !m.busy());
    }

    #[test]
    fn a_closed_gate_releases_a_pending_activation() {
        let (mut m, _) = pending();
        let outs = m.tick(t(10), false);
        assert_eq!(releases(&outs).len(), 1);
        assert!(m.is_idle());
    }

    #[test]
    fn an_activation_with_the_gate_closed_or_no_portal_is_released_at_once() {
        let mut m = Machine::new();
        let outs = m.activated(t(0), 1, Some((right_entry(), 0.5)), (1080.0, 840.0), false);
        assert_eq!(releases(&outs), vec![(1, (1078.0, 840.0))]);
        assert!(emits(&outs).is_empty());
        assert!(m.is_idle());
        assert!(m.deactivated(t(1), 1).is_empty());
        // No portal matches: released where it is.
        let outs = m.activated(t(2), 2, None, (500.0, 20.0), true);
        assert_eq!(releases(&outs), vec![(2, (500.0, 20.0))]);
        assert!(m.is_idle());
    }

    #[test]
    fn begin_adopts_the_pending_activation_and_started_precedes_the_events() {
        let (mut m, _) = pending();
        let _ = m.input(
            t(1),
            Some(1),
            Input::Key {
                code: 29,
                down: true,
            },
        ); // left ctrl
        let _ = m.input(
            t(1),
            Some(1),
            Input::Key {
                code: 42,
                down: true,
            },
        ); // left shift
        let _ = m.input(t(1), Some(1), Input::Frame);
        let begin = m.begin(t(50), CaptureId(9), PortalId(7), true, LOCKS);
        assert_eq!(
            begin.outs,
            vec![Out::Emit(CaptureEvent::Started { id: CaptureId(9) })]
        );
        let start = begin.result.unwrap();
        assert_eq!(
            start.held_keys,
            vec![HidUsage::keyboard(0xE0), HidUsage::keyboard(0xE1)]
        );
        assert_eq!(start.lock_keys, LOCKS);
        assert!(m.is_active());
        // From now on events are routed; a motion is scaled to device pixels of the display.
        let outs = m.input(t(60), Some(1), Input::Motion { dx: 3.0, dy: -1.0 });
        assert_eq!(
            emits(&outs),
            vec![&CaptureEvent::Motion {
                dx: 3.0,
                dy: -1.0,
                kind: MotionKind::Accelerated {
                    display: DisplayId(3)
                },
                at: t(60),
            }]
        );
    }

    #[test]
    fn begin_with_a_button_held_since_the_activation_fails_and_releases() {
        let (mut m, _) = pending();
        let _ = m.input(
            t(1),
            Some(1),
            Input::Button {
                code: BTN_LEFT,
                down: true,
            },
        );
        let begin = m.begin(t(5), CaptureId(1), PortalId(7), true, LOCKS);
        assert!(matches!(
            begin.result,
            Err(PlatformError::PointerButtonHeld)
        ));
        assert_eq!(releases(&begin.outs), vec![(1, (1078.0, 840.0))]);
        assert!(m.is_idle());
        // A button pressed and released again is fine.
        let (mut m, _) = pending();
        let _ = m.input(
            t(1),
            Some(1),
            Input::Button {
                code: BTN_LEFT,
                down: true,
            },
        );
        let _ = m.input(
            t(2),
            Some(1),
            Input::Button {
                code: BTN_LEFT,
                down: false,
            },
        );
        assert!(
            m.begin(t(5), CaptureId(1), PortalId(7), true, LOCKS)
                .result
                .is_ok()
        );
    }

    #[test]
    fn a_button_pressed_before_the_activation_is_unknown_and_its_release_is_forwarded() {
        // A3: its later release arrives as `down: false` for the router to drop.
        let (mut m, _) = pending();
        let _ = m.begin(t(5), CaptureId(1), PortalId(7), true, LOCKS);
        let outs = m.input(
            t(6),
            Some(1),
            Input::Button {
                code: BTN_LEFT,
                down: false,
            },
        );
        assert_eq!(
            emits(&outs),
            vec![&CaptureEvent::Button {
                button: MouseButton::PRIMARY,
                down: false,
                at: t(6)
            }]
        );
        let outs = m.input(
            t(7),
            Some(1),
            Input::Key {
                code: 30,
                down: false,
            },
        );
        assert_eq!(
            emits(&outs),
            vec![&CaptureEvent::Key {
                usage: HidUsage::keyboard(0x04),
                down: false,
                at: t(7)
            }]
        );
    }

    #[test]
    fn begin_while_locked_or_for_another_portal_or_with_nothing_pending_fails() {
        let (mut m, _) = pending();
        let begin = m.begin(t(5), CaptureId(1), PortalId(7), false, LOCKS);
        assert!(matches!(begin.result, Err(PlatformError::Locked)));
        assert_eq!(releases(&begin.outs).len(), 1);
        assert!(m.is_idle());

        let (mut m, _) = pending();
        let begin = m.begin(t(5), CaptureId(1), PortalId(99), true, LOCKS);
        assert!(matches!(begin.result, Err(PlatformError::NotFound)));
        assert_eq!(releases(&begin.outs).len(), 1);

        let mut m = Machine::new();
        let begin = m.begin(t(5), CaptureId(1), PortalId(7), true, LOCKS);
        assert!(matches!(begin.result, Err(PlatformError::NotFound)));
        assert!(begin.outs.is_empty());
        // With the gate closed, nothing pending is still "locked".
        let begin = m.begin(t(5), CaptureId(1), PortalId(7), false, LOCKS);
        assert!(matches!(begin.result, Err(PlatformError::Locked)));

        let (mut m, _) = pending();
        assert!(
            m.begin(t(5), CaptureId(1), PortalId(7), true, LOCKS)
                .result
                .is_ok()
        );
        let again = m.begin(t(6), CaptureId(2), PortalId(7), true, LOCKS);
        assert!(matches!(again.result, Err(PlatformError::Backend(_))));
        assert!(m.is_active());
    }

    #[test]
    fn an_active_capture_maps_keys_buttons_and_drops_repeats_and_unmapped_codes() {
        let (mut m, _) = pending();
        let _ = m.begin(t(5), CaptureId(1), PortalId(7), true, LOCKS);
        let key = |m: &mut Machine, code, down| m.input(t(9), Some(1), Input::Key { code, down });
        assert_eq!(emits(&key(&mut m, 30, true)).len(), 1);
        assert!(
            key(&mut m, 30, true).is_empty(),
            "a repeated down is not a press"
        );
        assert_eq!(emits(&key(&mut m, 30, false)).len(), 1);
        assert!(key(&mut m, 0x2FF, true).is_empty(), "no HID usage");
        let btn =
            |m: &mut Machine, code, down| m.input(t(9), Some(1), Input::Button { code, down });
        assert_eq!(emits(&btn(&mut m, BTN_RIGHT, true)).len(), 1);
        assert!(btn(&mut m, BTN_RIGHT, true).is_empty());
        assert!(btn(&mut m, 0x150, true).is_empty(), "unmapped button");
        assert!(matches!(
            emits(&btn(&mut m, BTN_EXTRA, true)).as_slice(),
            [CaptureEvent::Button { button, down: true, .. }] if *button == MouseButton::FORWARD
        ));
    }

    #[test]
    fn scrolls_map_to_crosspane_deltas() {
        let (mut m, _) = pending();
        let _ = m.begin(t(5), CaptureId(1), PortalId(7), true, LOCKS);
        let scroll = |m: &mut Machine, s| match m.input(t(9), Some(1), Input::Scroll(s)).as_slice()
        {
            [Out::Emit(CaptureEvent::Scroll { delta, .. })] => *delta,
            other => panic!("expected one scroll, got {other:?}"),
        };
        // EIS down is positive; Crosspane up is positive.
        let d = scroll(&mut m, Scroll::Discrete { dx: 120, dy: 240 });
        assert_eq!(
            (d.v120_x, d.v120_y, d.phase),
            (120, -240, ScrollPhase::Discrete)
        );
        assert!(d.pixels.is_none());
        let d = scroll(&mut m, Scroll::Smooth { dx: 2.0, dy: 5.0 });
        assert_eq!(d.pixels, Some(VectorLogical::new(2.0, -5.0)));
        assert_eq!(d.phase, ScrollPhase::Began);
        let d = scroll(&mut m, Scroll::Smooth { dx: 0.0, dy: 1.0 });
        assert_eq!(d.phase, ScrollPhase::Changed);
        // Stopping the axis that scrolls ends the gesture only when the other one is idle too.
        let d = scroll(&mut m, Scroll::Stop { x: true, y: false });
        assert_eq!(
            (d.phase, d.stop_x, d.stop_y),
            (ScrollPhase::Changed, true, false)
        );
        let d = scroll(&mut m, Scroll::Stop { x: false, y: true });
        assert_eq!((d.phase, d.stop_y), (ScrollPhase::Ended, true));
        let _ = scroll(&mut m, Scroll::Smooth { dx: 0.0, dy: 1.0 });
        let d = scroll(&mut m, Scroll::Cancel { x: true, y: true });
        assert_eq!(d.phase, ScrollPhase::Cancelled);
    }

    #[test]
    fn end_releases_to_the_requested_point_and_reports_requested() {
        let (mut m, _) = pending();
        let _ = m.begin(t(5), CaptureId(4), PortalId(7), true, LOCKS);
        let outs = m.end(t(80), (1000.0, 700.0));
        assert_eq!(
            outs,
            vec![
                Out::Release {
                    activation: 1,
                    at: (1000.0, 700.0)
                },
                Out::Emit(CaptureEvent::Ended {
                    id: CaptureId(4),
                    reason: EndReason::Requested
                }),
                // The pointer is off the portal again: the engine may arm it.
                Out::Emit(CaptureEvent::EdgeReleased {
                    portal: PortalId(7),
                    at: t(80)
                }),
            ]
        );
        assert!(m.is_idle());
        // Idempotent, and our own release is expected.
        assert!(m.end(t(81), (0.0, 0.0)).is_empty());
        assert!(m.deactivated(t(82), 1).is_empty());
        // Input after the end is stale.
        assert!(
            m.input(t(83), Some(1), Input::Motion { dx: 1.0, dy: 0.0 })
                .is_empty()
        );
        assert_eq!(m.dropped(), 1);
    }

    #[test]
    fn a_deactivation_we_did_not_ask_for_loses_the_capture_and_asks_to_re_arm() {
        let (mut m, _) = pending();
        let _ = m.begin(t(5), CaptureId(4), PortalId(7), true, LOCKS);
        let outs = m.deactivated(t(40), 1);
        assert_eq!(
            outs,
            vec![
                Out::Emit(CaptureEvent::Ended {
                    id: CaptureId(4),
                    reason: EndReason::Lost
                }),
                Out::Emit(CaptureEvent::EdgeReleased {
                    portal: PortalId(7),
                    at: t(40)
                }),
                Out::Rearm,
            ]
        );
        assert!(m.is_idle());
        // The same for a pending activation (escape chord before the begin).
        let (mut m, _) = pending();
        let outs = m.deactivated(t(40), 1);
        assert!(matches!(
            outs.as_slice(),
            [Out::Emit(CaptureEvent::EdgeReleased { .. }), Out::Rearm]
        ));
        // And one for an activation nobody knows is ignored.
        assert!(m.deactivated(t(41), 9).is_empty());
    }

    #[test]
    fn a_closed_gate_ends_an_active_capture_lost_after_releasing_it() {
        let (mut m, _) = pending();
        let _ = m.begin(t(5), CaptureId(4), PortalId(7), true, LOCKS);
        let outs = m.tick(t(30), false);
        assert_eq!(
            outs,
            vec![
                Out::Release {
                    activation: 1,
                    at: (1078.0, 840.0)
                },
                Out::Emit(CaptureEvent::Ended {
                    id: CaptureId(4),
                    reason: EndReason::Lost
                }),
                Out::Emit(CaptureEvent::EdgeReleased {
                    portal: PortalId(7),
                    at: t(30)
                }),
            ]
        );
        assert!(m.is_idle());
        // An open gate leaves it alone.
        let (mut m, _) = pending();
        let _ = m.begin(t(5), CaptureId(4), PortalId(7), true, LOCKS);
        assert!(m.tick(t(60_000), true).is_empty());
    }

    #[test]
    fn input_that_outruns_the_signal_is_replayed_and_stale_input_is_dropped() {
        let mut m = Machine::new();
        // The EIS socket was faster than the D-Bus signal: activation 1's key and a frame.
        assert!(
            m.input(
                t(0),
                Some(1),
                Input::Key {
                    code: 30,
                    down: true
                }
            )
            .is_empty()
        );
        assert!(m.input(t(0), Some(1), Input::Frame).is_empty());
        let outs = m.activated(t(2), 1, Some((right_entry(), 0.5)), (1080.0, 840.0), true);
        assert_eq!(pressed(&outs), vec![0.5]);
        // The key is in the ledger: it shows up as a held key at begin.
        let begin = m.begin(t(9), CaptureId(1), PortalId(7), true, LOCKS);
        assert_eq!(
            begin.result.unwrap().held_keys,
            vec![HidUsage::keyboard(0x04)]
        );
        // Input without a sequence and input of an older activation is dropped.
        assert!(
            m.input(t(10), None, Input::Motion { dx: 1.0, dy: 1.0 })
                .is_empty()
        );
        let _ = m.end(t(11), (0.0, 0.0));
        assert!(
            m.input(
                t(12),
                Some(1),
                Input::Key {
                    code: 31,
                    down: true
                }
            )
            .is_empty()
        );
        assert_eq!(m.dropped(), 2);
        // Input of the next activation that outruns its signal waits for it.
        assert!(
            m.input(
                t(13),
                Some(2),
                Input::Button {
                    code: BTN_LEFT,
                    down: true
                }
            )
            .is_empty()
        );
        let _ = m.deactivated(t(14), 1);
        let outs = m.activated(t(15), 2, Some((right_entry(), 0.5)), (1080.0, 840.0), true);
        assert_eq!(pressed(&outs), vec![0.5]);
        let begin = m.begin(t(16), CaptureId(2), PortalId(7), true, LOCKS);
        assert!(matches!(
            begin.result,
            Err(PlatformError::PointerButtonHeld)
        ));
    }

    #[test]
    fn a_new_activation_replaces_one_whose_deactivation_was_missed() {
        let (mut m, _) = pending();
        let outs = m.activated(t(100), 2, Some((right_entry(), 0.1)), (1080.0, 650.0), true);
        assert!(matches!(
            emits(&outs).as_slice(),
            [
                CaptureEvent::EdgeReleased { .. },
                CaptureEvent::EdgePressed { .. }
            ]
        ));
        assert_eq!(m.live().map(|(id, _)| id), Some(2));
        // The old activation's input is stale.
        assert!(m.input(t(101), Some(1), Input::Frame).is_empty());
    }

    #[test]
    fn portal_removal_and_loss_and_abort() {
        // Removed portal, pending.
        let (mut m, _) = pending();
        assert!(m.portal_gone(t(1), |id| id == PortalId(7)).is_empty());
        let outs = m.portal_gone(t(2), |_| false);
        assert_eq!(releases(&outs).len(), 1);
        assert!(m.is_idle());
        // Removed portal, active: Release then Lost.
        let (mut m, _) = pending();
        let _ = m.begin(t(5), CaptureId(3), PortalId(7), true, LOCKS);
        let outs = m.portal_gone(t(6), |_| false);
        assert!(matches!(
            outs.as_slice(),
            [
                Out::Release { activation: 1, .. },
                Out::Emit(CaptureEvent::Ended {
                    id: CaptureId(3),
                    reason: EndReason::Lost
                }),
                Out::Emit(CaptureEvent::EdgeReleased { .. })
            ]
        ));
        // The session went away: Lost without a release.
        let (mut m, _) = pending();
        let _ = m.begin(t(5), CaptureId(3), PortalId(7), true, LOCKS);
        assert_eq!(
            m.lost(t(6)),
            vec![
                Out::Emit(CaptureEvent::Ended {
                    id: CaptureId(3),
                    reason: EndReason::Lost
                }),
                Out::Emit(CaptureEvent::EdgeReleased {
                    portal: PortalId(7),
                    at: t(6)
                }),
            ]
        );
        // Abort: release plus Aborted, and a pending one is released too.
        let (mut m, _) = pending();
        let _ = m.begin(t(5), CaptureId(3), PortalId(7), true, LOCKS);
        let outs = m.abort(t(6));
        assert_eq!(releases(&outs), vec![(1, (1078.0, 840.0))]);
        assert!(matches!(
            emits(&outs).as_slice(),
            [
                CaptureEvent::Ended {
                    reason: EndReason::Aborted,
                    ..
                },
                CaptureEvent::EdgeReleased { .. }
            ]
        ));
        let (mut m, _) = pending();
        let outs = m.abort(t(6));
        assert_eq!(releases(&outs).len(), 1);
        assert!(m.abort(t(7)).is_empty());
    }
}
