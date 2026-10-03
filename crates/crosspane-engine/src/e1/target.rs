//! The target role: acceptance, journaled injection, leases and local override.

use core::fmt;
use core::time::Duration;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use crosspane_input::Held;
use crosspane_input::journal::{Journal, JournalError};
use crosspane_input::lease::{Action, TargetLedger};
use crosspane_platform::{
    CaptureEvent, LockState, Overlay, OverlayAnchor, Rgb8, SessionEvent, SessionState,
};
use crosspane_protocol::link::LinkEvent;
use crosspane_protocol::msg::{
    Capability, ControlMessage, EndReason, InputMessage, PointerMessage, Refusal, TargetStatus,
};
use crosspane_types::geom::PointDevice;
use crosspane_types::hid::MouseButton;
use crosspane_types::id::DisplayId;
use crosspane_types::id::{NodeId, SessionId};
use crosspane_types::time::MonoTime;

use crate::config::EngineConfig;
use crate::io::{
    Command, InjectCmd, InjectId, Input, Notice, Output, ProjectionKey, TARGET_INDICATOR,
};
use crate::physical_input::{Owner, PhysicalInput, ReleaseAction};

const RETRY_INTERVAL: Duration = Duration::from_millis(50);

type JournalState = (Box<dyn Journal>, BTreeSet<Held>);

#[derive(Clone)]
struct TrackedJournal(Arc<Mutex<JournalState>>);

impl Journal for TrackedJournal {
    fn record_down(&mut self, item: Held) -> Result<(), JournalError> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| std::io::Error::other("E1 journal mutex poisoned"))?;
        let result = state.0.record_down(item);
        state.1.insert(item); // A failed write may have persisted before reporting failure.
        result
    }

    fn record_up(&mut self, item: Held) -> Result<(), JournalError> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| std::io::Error::other("E1 journal mutex poisoned"))?;
        state.0.record_up(item)?;
        state.1.remove(&item);
        Ok(())
    }

    fn held(&self) -> Result<Vec<Held>, JournalError> {
        let state = self
            .0
            .lock()
            .map_err(|_| std::io::Error::other("E1 journal mutex poisoned"))?;
        Ok(state.1.iter().copied().collect())
    }
}

#[derive(Clone, Copy, Debug)]
struct ActiveSession {
    controller: NodeId,
    session: SessionId,
    last_motion_seq: u32,
}

#[derive(Debug)]
enum Pending {
    Release(Vec<(Held, u64)>),
    Recover(Vec<Held>),
}

#[derive(Debug)]
struct DragPress {
    key: ProjectionKey,
    token: u32,
    move_id: Option<InjectId>,
    until: MonoTime,
    queue: VecDeque<Input>,
}

/// The target side of E1: accepting control, injecting through the lease ledger, local override.
pub struct TargetE1 {
    state: SessionState,
    asleep: bool,
    awaiting_state: bool,
    grants: BTreeMap<NodeId, BTreeSet<Capability>>,
    ledger: TargetLedger<TrackedJournal>,
    journal: TrackedJournal,
    physical: PhysicalInput,
    active: Option<ActiveSession>,
    pending: BTreeMap<InjectId, Pending>,
    next_id: u64,
    now: MonoTime,
    recovery: Vec<Held>,
    // A completion for an earlier release must never clear a later press's journal record.
    generations: BTreeMap<Held, u64>,
    unconfirmed: BTreeMap<Held, u64>,
    release_retry: BTreeMap<Held, (MonoTime, InjectId)>,
    drag_offer: Option<(ProjectionKey, u32, DisplayId, PointDevice)>,
    drag_press: Option<DragPress>,
    drag_arm: Option<(ProjectionKey, u32, MonoTime)>,
    drag_ignore_up: Option<SessionId>,
    drag_used: Option<(ProjectionKey, u32)>,
    drag_down: Option<InjectId>,
}

impl fmt::Debug for TargetE1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TargetE1")
            .field("state", &self.state)
            .field("asleep", &self.asleep)
            .field("awaiting_state", &self.awaiting_state)
            .field("active", &self.active)
            .field("pending_count", &self.pending.len())
            .field("recovery_count", &self.recovery.len())
            .field("unconfirmed_count", &self.unconfirmed.len())
            .finish_non_exhaustive()
    }
}

impl TargetE1 {
    /// Opens the journal; the returned outputs release whatever a crashed previous process left
    /// held (04 §8 invariant 2).
    pub fn new(
        _config: &EngineConfig,
        journal: Box<dyn Journal>,
        now: MonoTime,
    ) -> Result<(TargetE1, Vec<Output>), JournalError> {
        let held = journal.held()?.into_iter().collect();
        let journal = TrackedJournal(Arc::new(Mutex::new((journal, held))));
        let (ledger, recovery) = TargetLedger::open(journal.clone())?;
        let mut target = Self {
            state: SessionState {
                lock: LockState::Locked,
                active: None,
            },
            asleep: false,
            awaiting_state: true,
            grants: BTreeMap::new(),
            ledger,
            journal,
            physical: PhysicalInput::default(),
            active: None,
            pending: BTreeMap::new(),
            next_id: 1,
            now,
            recovery,
            generations: BTreeMap::new(),
            unconfirmed: BTreeMap::new(),
            release_retry: BTreeMap::new(),
            drag_offer: None,
            drag_press: None,
            drag_arm: None,
            drag_ignore_up: None,
            drag_used: None,
            drag_down: None,
        };
        let mut out = Vec::new();
        target.physical.startup(true, &target.recovery)?;
        target.recover(&mut out)?;
        Ok((target, out))
    }

    /// Engine construction attaches the node's shared coordinator before normal input.
    pub(crate) fn set_physical(
        &mut self,
        physical: PhysicalInput,
        out: &mut Vec<Output>,
    ) -> Result<(), JournalError> {
        self.physical = physical;
        self.pending.clear();
        self.release_retry.clear();
        self.physical.startup(true, &self.recovery)?;
        self.recover(out)
    }

    fn owner(&self) -> Option<Owner> {
        self.active.map(|s| Owner::E1(s.controller, s.session))
    }

    /// Handle one input (every input is offered to both roles), appending outputs.
    pub fn handle(&mut self, input: &Input, now: MonoTime, out: &mut Vec<Output>) {
        self.now = now;
        for item in self.physical.uncertain(input, now) {
            self.cleanup_item(item, now, out);
        }
        if self.drag_press.as_ref().is_some_and(|p| now >= p.until) {
            self.end_session(Some(EndReason::Released), None, out);
        }
        if self.drag_arm.is_some_and(|(_, _, until)| now >= until)
            && let Some((key, token, _)) = self.drag_arm.take()
        {
            out.push(Output::DisarmDrag { key, token });
        }
        if self.drag_input(input, out) {
            self.collect();
            return;
        }
        match input {
            Input::Session(event) => {
                match event {
                    SessionEvent::State(state) => {
                        self.state = *state;
                        self.awaiting_state = false;
                    }
                    SessionEvent::WillSleep => self.asleep = true,
                    SessionEvent::Woke => {
                        self.asleep = false;
                        self.awaiting_state = true;
                    }
                    _ => {}
                }
                if !self.permits_io() {
                    let notice = self.active.map(|s| Notice::TargetLocked(s.controller));
                    self.end_session(Some(EndReason::TargetLocked), notice, out);
                }
            }
            Input::Grants(grants) => {
                self.grants = grants.clone();
                if self.active.is_some_and(|s| !self.has_grant(s.controller)) {
                    self.end_session(Some(EndReason::Revoked), None, out);
                }
            }
            Input::Link(LinkEvent::Control { peer, msg }) => match msg {
                ControlMessage::StartControl {
                    session,
                    entry_display,
                    entry,
                    lock_keys,
                } => {
                    let refusal = if !self.has_grant(*peer) {
                        Some(Refusal::Permission)
                    } else if !self.permits_io() {
                        Some(Refusal::Locked)
                    } else if self.active.is_some_and(|s| s.controller != *peer) {
                        Some(Refusal::Busy)
                    } else {
                        None
                    };
                    if let Some(reason) = refusal {
                        out.push(Output::SendControl {
                            peer: *peer,
                            msg: ControlMessage::ControlRefused {
                                session: *session,
                                reason,
                            },
                        });
                        out.push(Output::Notice(Notice::Refused {
                            peer: *peer,
                            reason,
                        }));
                        return;
                    }
                    self.end_session(Some(EndReason::Released), None, out);
                    self.active = Some(ActiveSession {
                        controller: *peer,
                        session: *session,
                        last_motion_seq: 0,
                    });
                    self.inject(
                        InjectCmd::MoveTo {
                            display: *entry_display,
                            position: *entry,
                        },
                        None,
                        out,
                    );
                    self.inject(InjectCmd::LockKeys(*lock_keys), None, out);
                    out.push(Output::ShowOverlay {
                        id: TARGET_INDICATOR,
                        overlay: Overlay {
                            display: *entry_display,
                            anchor: OverlayAnchor::TopCenter,
                            text: format!("Controlled from {}", peer.short()),
                            accent: Rgb8 {
                                r: 0x3b,
                                g: 0x82,
                                b: 0xf6,
                            },
                        },
                    });
                    out.push(Output::MonitorLocalActivity(true));
                    out.push(Output::SendControl {
                        peer: *peer,
                        msg: ControlMessage::ControlStarted { session: *session },
                    });
                    out.push(Output::Notice(Notice::ControlledBy(*peer)));
                }
                ControlMessage::EndControl { session, .. } if self.matches(*peer, *session) => {
                    self.end_session(None, None, out);
                }
                _ => {}
            },
            Input::Link(LinkEvent::Input { peer, msg }) => self.input_message(*peer, msg, out),
            Input::Link(LinkEvent::Motion { peer, msg }) => self.motion(*peer, msg, out),
            Input::Link(LinkEvent::Closed { peer, .. })
                if self.active.is_some_and(|s| s.controller == *peer) =>
            {
                self.end_session(None, None, out);
            }
            Input::Capture(CaptureEvent::LocalActivity { .. }) => self.local_activity(out),
            Input::Tick => self.tick(out),
            Input::Command(Command::Panic) => {
                self.end_session(Some(EndReason::Panic), None, out);
            }
            Input::InjectDone { id, ok } => self.inject_done(*id, *ok, out),
            _ => {}
        }
        self.collect();
    }

    /// True while another node controls this one (an E1 session is active here).
    /// Whether a `StartControl` from `peer` would be admitted now (the same checks as the
    /// admission itself): only then may it pre-empt this node's own pending crossing.
    pub(crate) fn would_admit(&self, peer: NodeId) -> bool {
        self.has_grant(peer)
            && self.permits_io()
            && self.active.is_none_or(|s| s.controller == peer)
    }

    pub fn is_controlled(&self) -> bool {
        self.active.is_some()
    }

    /// WP-2.43 §2.3 step 1: no active session, `recovery` empty, `unconfirmed` empty,
    /// `release_retry` empty, nothing held through the ledger: this injector holds nothing and has
    /// no release outstanding.
    pub(crate) fn settled(&self) -> bool {
        self.active.is_none()
            && self.recovery.is_empty()
            && self.unconfirmed.is_empty()
            && self.release_retry.is_empty()
            && self.ledger.held().is_empty()
    }

    /// The node controlling this one, if any.
    pub fn controller(&self) -> Option<NodeId> {
        self.active.map(|s| s.controller)
    }

    pub fn next_deadline(&self) -> Option<MonoTime> {
        [
            self.ledger.next_deadline(),
            self.release_retry
                .values()
                .map(|(deadline, _)| *deadline)
                .min(),
            self.physical.next_deadline().ok().flatten(),
            self.drag_press.as_ref().map(|p| p.until),
            self.drag_arm.map(|(_, _, until)| until),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn permits_io(&self) -> bool {
        self.state.permits_io() && !self.asleep && !self.awaiting_state
    }

    fn has_grant(&self, peer: NodeId) -> bool {
        self.grants
            .get(&peer)
            .is_some_and(|grants| grants.contains(&Capability::InputAccept))
    }

    fn matches(&self, peer: NodeId, session: SessionId) -> bool {
        self.active
            .is_some_and(|s| s.controller == peer && s.session == session)
    }

    fn inject(&mut self, cmd: InjectCmd, pending: Option<Pending>, out: &mut Vec<Output>) {
        let release = matches!(
            cmd,
            InjectCmd::Key { down: false, .. }
                | InjectCmd::Button { down: false, .. }
                | InjectCmd::ReleaseAll
                | InjectCmd::Recover { .. }
        );
        if !release && !self.permits_io() {
            return;
        }
        let id = InjectId(self.next_id);
        self.next_id += 1;
        if let Some(pending) = pending {
            self.pending.insert(id, pending);
        }
        out.push(Output::Inject { id, cmd });
    }

    fn action(&mut self, action: Action, out: &mut Vec<Output>) {
        let requested = InjectId(self.next_id);
        self.next_id += 1; // Absorbed actions also consume a distinct logical generation.
        let (item, down) = match action {
            Action::Press(item) => {
                self.generations.insert(item, requested.0);
                self.unconfirmed.remove(&item);
                self.release_retry.remove(&item);
                let Some(owner) = self.owner() else { return };
                if !self
                    .physical
                    .press(
                        owner,
                        item,
                        requested,
                        Some(self.now.saturating_add(Duration::from_millis(500))),
                    )
                    .unwrap_or(false)
                {
                    return;
                }
                (item, true)
            }
            Action::Release(item) => {
                if item == Held::Button(MouseButton::PRIMARY) {
                    self.drag_down = None;
                }
                let generation = self.generation(item);
                self.unconfirmed.insert(item, generation);
                match self
                    .physical
                    .release(self.owner(), item, requested, self.now)
                {
                    Ok(ReleaseAction::Absorb) => {
                        if self.ledger.confirm_released(&[item]).is_ok() {
                            self.unconfirmed.remove(&item);
                            self.release_retry.remove(&item);
                        } else {
                            self.release_retry
                                .insert(item, (self.now.saturating_add(RETRY_INTERVAL), requested));
                            self.end_session(Some(EndReason::Released), None, out);
                        }
                        return;
                    }
                    Ok(ReleaseAction::Submit { id, until, emit }) => {
                        self.release_retry.insert(item, (until, id));
                        if let Pending::Release(items) = self
                            .pending
                            .entry(id)
                            .or_insert(Pending::Release(Vec::new()))
                            && !items.contains(&(item, generation))
                        {
                            items.push((item, generation));
                        }
                        if emit {
                            let cmd = match item {
                                Held::Key(usage) => InjectCmd::Key { usage, down: false },
                                Held::Button(button) => InjectCmd::Button {
                                    button,
                                    down: false,
                                },
                            };
                            out.push(Output::Inject { id, cmd });
                        }
                        return;
                    }
                    Err(_) => {
                        self.journal_failed(out);
                        return;
                    }
                }
            }
        };
        let cmd = match item {
            Held::Key(usage) => InjectCmd::Key { usage, down },
            Held::Button(button) => InjectCmd::Button { button, down },
        };
        out.push(Output::Inject { id: requested, cmd });
    }

    pub(crate) fn cleanup_item(&mut self, item: Held, now: MonoTime, out: &mut Vec<Output>) {
        self.now = now;
        let failed_drag = item == Held::Button(MouseButton::PRIMARY)
            && self.drag_down.take() == Some(InjectId(self.generation(item)));
        let _ = self.physical.cleanup(item);
        if let Some(press) = &mut self.drag_press {
            press.queue.retain(|input| !matches!(input, Input::Link(LinkEvent::Input { msg: InputMessage::Key { usage, .. }, .. }) if item == Held::Key(*usage))
                && !matches!(input, Input::Link(LinkEvent::Input { msg: InputMessage::Button { button, .. }, .. }) if item == Held::Button(*button)));
        }
        self.key_or_button(item, false, out);
        self.collect();
        if failed_drag {
            self.end_session(Some(EndReason::Released), None, out);
        }
    }

    fn collect(&mut self) {
        self.pending.retain(|id, pending| {
            if let Pending::Release(items) = pending {
                items.retain(|(item, generation)| {
                    self.unconfirmed.get(item) == Some(generation)
                        && self.physical.release_member(*item, *id, true)
                });
                !items.is_empty()
            } else if let Pending::Recover(items) = pending {
                items.retain(|item| {
                    self.recovery.contains(item) && self.physical.release_member(*item, *id, true)
                });
                !items.is_empty()
            } else {
                true
            }
        });
        let _ = self
            .physical
            .retain_blocks(true, self.unconfirmed.keys().copied().collect());
    }

    fn generation(&self, item: Held) -> u64 {
        self.generations.get(&item).copied().unwrap_or(0)
    }

    fn release_all(&mut self, _force: bool, out: &mut Vec<Output>) {
        for action in self.ledger.release_all() {
            self.action(action, out);
        }
    }

    fn end_session(
        &mut self,
        reason: Option<EndReason>,
        notice: Option<Notice>,
        out: &mut Vec<Output>,
    ) {
        self.cancel_drag(false, out);
        self.drag_ignore_up = None;
        let Some(active) = self.active else {
            return;
        };
        self.release_all(false, out);
        out.push(Output::HideOverlay(TARGET_INDICATOR));
        out.push(Output::MonitorLocalActivity(false));
        if let Some(reason) = reason {
            out.push(Output::SendControl {
                peer: active.controller,
                msg: ControlMessage::EndControl {
                    session: active.session,
                    reason,
                },
            });
        }
        out.push(Output::Notice(
            notice.unwrap_or(Notice::ControlEnded(active.controller)),
        ));
        self.active = None;
    }

    fn journal_failed(&mut self, out: &mut Vec<Output>) {
        // Reopen tracked attempts, including a torn record that TargetLedger never accepted.
        if let Ok((ledger, items)) = TargetLedger::open(self.journal.clone()) {
            self.ledger = ledger;
            for item in items {
                self.action(Action::Release(item), out);
            }
        }
        self.end_session(Some(EndReason::Released), None, out);
    }

    fn input_message(&mut self, peer: NodeId, msg: &InputMessage, out: &mut Vec<Output>) {
        let (session, seq) = match msg {
            InputMessage::Key { session, seq, .. }
            | InputMessage::Button { session, seq, .. }
            | InputMessage::Scroll { session, seq, .. }
            | InputMessage::LockKeys { session, seq, .. }
            | InputMessage::State { session, seq, .. } => (*session, *seq),
            // E2 projection input is handled by the e2 role (WP-2.5), never by E1.
            InputMessage::Ack { .. }
            | InputMessage::Status { .. }
            | InputMessage::Proj(_)
            | InputMessage::PressAt { .. } => {
                return;
            }
        };
        if !self.matches(peer, session) {
            return;
        }
        match msg {
            InputMessage::Key { usage, down, .. } => {
                self.key_or_button(Held::Key(*usage), *down, out);
            }
            InputMessage::Button { button, down, .. } => {
                self.key_or_button(Held::Button(*button), *down, out);
            }
            InputMessage::Scroll { delta, .. } => {
                self.inject(InjectCmd::Scroll(*delta), None, out);
            }
            InputMessage::LockKeys { keys, .. } => {
                self.inject(InjectCmd::LockKeys(*keys), None, out);
            }
            InputMessage::State {
                held_keys,
                held_buttons,
                ..
            } => {
                let listed: Vec<_> = held_keys
                    .iter()
                    .copied()
                    .map(Held::Key)
                    .chain(held_buttons.iter().copied().map(Held::Button))
                    .collect();
                for action in self.ledger.on_heartbeat(&listed, self.now) {
                    self.action(action, out);
                }
            }
            _ => {}
        }
        out.push(Output::SendInput {
            peer,
            msg: InputMessage::Ack { session, seq },
        });
    }

    pub(crate) fn prepare_drag(
        &mut self,
        key: ProjectionKey,
        token: u32,
        display: DisplayId,
        position: PointDevice,
        out: &mut Vec<Output>,
    ) {
        if self.active.is_some_and(|s| s.controller == key.source) {
            self.cancel_drag(true, out);
            self.drag_offer = Some((key, token, display, position));
        }
    }

    pub(crate) fn drag_ended(&mut self, key: ProjectionKey, out: &mut Vec<Output>) {
        if self.drag_key().is_some_and(|(known, _)| known == key) {
            self.cancel_continuation(out);
        }
    }

    fn cancel_continuation(&mut self, out: &mut Vec<Output>) {
        if self.drag_press.is_some() || self.drag_used.is_some() {
            self.end_session(Some(EndReason::Released), None, out);
        } else {
            self.cancel_drag(true, out);
        }
    }

    fn drag_key(&self) -> Option<(ProjectionKey, u32)> {
        self.drag_offer
            .map(|(key, token, ..)| (key, token))
            .or_else(|| self.drag_press.as_ref().map(|p| (p.key, p.token)))
            .or(self.drag_used)
    }

    fn cancel_drag(&mut self, flush: bool, out: &mut Vec<Output>) {
        let press = self.drag_press.take();
        let held = press.is_some() || self.drag_used.take().is_some();
        self.drag_down = None;
        self.drag_offer = None;
        if let Some((key, token, _)) = self.drag_arm.take() {
            out.push(Output::DisarmDrag { key, token });
        } else if let Some(p) = &press {
            out.push(Output::DisarmDrag {
                key: p.key,
                token: p.token,
            });
        }
        if press.is_some() {
            self.drag_ignore_up = self.active.map(|s| s.session);
        }
        if flush && held {
            self.key_or_button(Held::Button(MouseButton::PRIMARY), false, out);
            if let Some(p) = press {
                self.flush_drag(p.queue, out);
            }
        }
    }

    fn flush_drag(&mut self, queue: VecDeque<Input>, out: &mut Vec<Output>) {
        for input in queue {
            self.handle(&input, self.now, out);
        }
    }

    fn drag_ack(&self, seq: u32, out: &mut Vec<Output>) {
        if let Some(active) = self.active {
            out.push(Output::SendInput {
                peer: active.controller,
                msg: InputMessage::Ack {
                    session: active.session,
                    seq,
                },
            });
        }
    }

    fn drag_input(&mut self, input: &Input, out: &mut Vec<Output>) -> bool {
        if self
            .drag_press
            .as_ref()
            .is_some_and(|p| p.queue.len() >= 256)
        {
            self.end_session(Some(EndReason::Released), None, out);
        }
        match input {
            Input::InjectDone { id, ok }
                if self.drag_down == Some(*id)
                    && self.generation(Held::Button(MouseButton::PRIMARY)) == id.0
                    && self
                        .ledger
                        .held()
                        .contains(&Held::Button(MouseButton::PRIMARY)) =>
            {
                self.drag_down = None;
                if !ok {
                    self.release_all(true, out);
                    self.end_session(Some(EndReason::Released), None, out);
                }
                true
            }
            Input::InjectDone { id, ok }
                if self
                    .drag_press
                    .as_ref()
                    .is_some_and(|p| p.move_id == Some(*id)) =>
            {
                if *ok && self.physical.can_drag(self.owner()) {
                    if let Some(p) = &mut self.drag_press {
                        p.move_id = None;
                        let until = self.now.saturating_add(Duration::from_secs(2));
                        self.drag_arm = Some((p.key, p.token, until));
                        out.push(Output::ArmDrag {
                            key: p.key,
                            token: p.token,
                            until,
                        });
                    }
                } else {
                    self.end_session(Some(EndReason::Released), None, out);
                }
                true
            }
            Input::DragArmed { key, token, ok } => {
                if self
                    .drag_press
                    .as_ref()
                    .is_some_and(|p| p.key == *key && p.token == *token && p.move_id.is_none())
                {
                    if *ok && self.physical.can_drag(self.owner()) {
                        if let Some(p) = self.drag_press.take() {
                            self.drag_used = Some((p.key, p.token));
                            let first = out.len();
                            self.key_or_button(Held::Button(MouseButton::PRIMARY), true, out);
                            self.drag_down = out[first..].iter().find_map(|output| match output {
                                Output::Inject {
                                    id,
                                    cmd:
                                        InjectCmd::Button {
                                            button: MouseButton::PRIMARY,
                                            down: true,
                                        },
                                } => Some(*id),
                                _ => None,
                            });
                            self.flush_drag(p.queue, out);
                        }
                    } else {
                        self.end_session(Some(EndReason::Released), None, out);
                    }
                } else if *ok
                    && !self
                        .drag_arm
                        .is_some_and(|(known, expected, _)| known == *key && expected == *token)
                {
                    out.push(Output::DisarmDrag {
                        key: *key,
                        token: *token,
                    });
                }
                true
            }
            Input::Link(LinkEvent::Control {
                peer,
                msg:
                    ControlMessage::Projection(
                        crosspane_protocol::projection::ProjectionMessage::DragCancel {
                            projection,
                            token,
                        },
                    ),
            }) => {
                let key = ProjectionKey {
                    source: *peer,
                    projection: *projection,
                };
                if self.drag_key() == Some((key, *token)) {
                    self.cancel_continuation(out);
                }
                false
            }
            Input::Link(LinkEvent::Input {
                peer,
                msg:
                    InputMessage::PressAt {
                        session,
                        seq,
                        button,
                        display,
                        position,
                    },
            }) => {
                if self.matches(*peer, *session)
                    && *button == MouseButton::PRIMARY
                    && self.permits_io()
                    && self.drag_offer.is_some_and(|(key, _, expected, point)| {
                        key.source == *peer && expected == *display && point == *position
                    })
                    && let Some((key, token, _, _)) = self.drag_offer.take()
                {
                    self.drag_ignore_up = None;
                    self.drag_ack(*seq, out);
                    if !self.physical.can_drag(self.owner()) {
                        self.end_session(Some(EndReason::Released), None, out);
                        return true;
                    }
                    let id = InjectId(self.next_id);
                    self.drag_press = Some(DragPress {
                        key,
                        token,
                        move_id: Some(id),
                        until: self.now.saturating_add(Duration::from_millis(500)),
                        queue: VecDeque::new(),
                    });
                    self.inject(
                        InjectCmd::MoveTo {
                            display: *display,
                            position: *position,
                        },
                        None,
                        out,
                    );
                }
                true
            }
            Input::Link(LinkEvent::Input { peer, msg })
                if self.active.is_some_and(|s| s.controller == *peer) =>
            {
                if let InputMessage::Button {
                    session,
                    seq,
                    button: MouseButton::PRIMARY,
                    down: false,
                } = msg
                    && self.matches(*peer, *session)
                    && (self.drag_press.is_some() || self.drag_ignore_up == Some(*session))
                {
                    self.cancel_drag(true, out);
                    self.drag_ignore_up = None;
                    self.drag_ack(*seq, out);
                    return true;
                }
                if self.drag_press.is_some() {
                    if let InputMessage::Key { session, seq, .. }
                    | InputMessage::Button { session, seq, .. }
                    | InputMessage::Scroll { session, seq, .. }
                    | InputMessage::LockKeys { session, seq, .. }
                    | InputMessage::State { session, seq, .. } = msg
                        && self.matches(*peer, *session)
                    {
                        self.drag_ack(*seq, out);
                        if matches!(msg, InputMessage::State { .. }) {
                            // Refresh contact without applying this queued heartbeat's releases
                            // ahead of earlier input. Its real listing is applied in FIFO order.
                            self.ledger.on_heartbeat(&self.ledger.held(), self.now);
                        }
                    }
                    let blocked = match msg {
                        InputMessage::Key {
                            usage, down: true, ..
                        } => self.physical.blocked(Held::Key(*usage)).unwrap_or(true),
                        InputMessage::Button {
                            button, down: true, ..
                        } => self.physical.blocked(Held::Button(*button)).unwrap_or(true),
                        _ => false,
                    };
                    if !blocked && let Some(p) = &mut self.drag_press {
                        p.queue.push_back(input.clone());
                    }
                    return true;
                }
                if matches!(msg, InputMessage::Button { session, button: MouseButton::PRIMARY, down: false, .. } if self.matches(*peer, *session))
                {
                    self.drag_used = None;
                    if let Some((key, token, _)) = self.drag_arm.take() {
                        out.push(Output::DisarmDrag { key, token });
                    }
                }
                false
            }
            Input::Link(LinkEvent::Motion { peer, msg })
                if self.matches(*peer, msg.session) && self.drag_press.is_some() =>
            {
                if let Some(p) = &mut self.drag_press {
                    p.queue.push_back(input.clone());
                }
                true
            }
            _ => false,
        }
    }

    fn key_or_button(&mut self, item: Held, down: bool, out: &mut Vec<Output>) {
        if down && (!self.permits_io() || self.physical.blocked(item).unwrap_or(true)) {
            return;
        }
        match self.ledger.on_input(item, down, self.now) {
            Ok(Some(action)) => self.action(action, out),
            Ok(None) => {}
            Err(_) => self.journal_failed(out),
        }
    }

    fn motion(&mut self, peer: NodeId, msg: &PointerMessage, out: &mut Vec<Output>) {
        if !self.matches(peer, msg.session) || !self.permits_io() {
            return;
        }
        if let Some(active) = &mut self.active {
            if msg.seq <= active.last_motion_seq {
                return;
            }
            active.last_motion_seq = msg.seq;
        }
        self.inject(
            InjectCmd::MoveTo {
                display: msg.display,
                position: msg.position,
            },
            None,
            out,
        );
    }

    fn local_activity(&mut self, out: &mut Vec<Output>) {
        let Some(active) = self.active else {
            return;
        };
        self.release_all(true, out);
        out.push(Output::SendInput {
            peer: active.controller,
            msg: InputMessage::Status {
                session: active.session,
                status: TargetStatus::LocalOverride,
            },
        });
        // The status ends control on the controller. Sending an EndControl on its separate
        // stream as well could arrive first and hide the handover's distinct notice.
        self.end_session(None, None, out);
    }

    fn tick(&mut self, out: &mut Vec<Output>) {
        for action in self.ledger.on_tick(self.now) {
            self.action(action, out);
        }
        let _ = self.recover(out);
        let due: Vec<_> = self
            .release_retry
            .iter()
            .filter(|(item, (deadline, _))| *deadline <= self.now && !self.recovery.contains(item))
            .map(|(&item, _)| item)
            .collect();
        for item in due {
            self.action(Action::Release(item), out);
        }
    }

    fn recover(&mut self, out: &mut Vec<Output>) -> Result<(), JournalError> {
        let items: Vec<_> = self
            .recovery
            .iter()
            .copied()
            .filter(|item| {
                self.release_retry
                    .get(item)
                    .is_none_or(|(until, _)| *until <= self.now)
            })
            .collect();
        if items.is_empty() {
            return Ok(());
        }
        let requested = InjectId(self.next_id);
        self.next_id += 1;
        for request in self.physical.recovery(&items, requested, self.now)? {
            if let Pending::Recover(pending) = self
                .pending
                .entry(request.id)
                .or_insert(Pending::Recover(Vec::new()))
            {
                for &(item, until) in &request.items {
                    self.unconfirmed.insert(item, 0);
                    self.release_retry.insert(item, (until, request.id));
                    if !pending.contains(&item) {
                        pending.push(item);
                    }
                }
            }
            out.extend(request.output());
        }
        self.collect();
        Ok(())
    }

    fn inject_done(&mut self, id: InjectId, ok: bool, out: &mut Vec<Output>) {
        match self.pending.remove(&id) {
            Some(Pending::Recover(items)) if !self.recovery.is_empty() => {
                for item in items {
                    if self.generation(item) != 0 || !self.physical.release_member(item, id, ok) {
                        continue;
                    }
                    if ok && self.ledger.recovered(&[item]).is_ok() {
                        self.recovery.retain(|known| *known != item);
                        self.unconfirmed.remove(&item);
                        self.release_retry.remove(&item);
                    } else {
                        self.physical.release_failed(item, id, self.now);
                        self.release_retry
                            .insert(item, (self.now.saturating_add(RETRY_INTERVAL), id));
                    }
                }
            }
            Some(Pending::Release(items)) => {
                let items: Vec<_> = items
                    .into_iter()
                    .filter(|(item, generation)| {
                        self.unconfirmed.get(item) == Some(generation)
                            && self.physical.release_member(*item, id, ok)
                    })
                    .map(|(item, _)| item)
                    .collect();
                let journal_failed = ok && self.ledger.confirm_released(&items).is_err();
                if ok && !journal_failed {
                    for item in items {
                        self.unconfirmed.remove(&item);
                        self.release_retry.remove(&item);
                    }
                } else {
                    for item in items {
                        self.physical.release_failed(item, id, self.now);
                        self.release_retry
                            .insert(item, (self.now.saturating_add(RETRY_INTERVAL), id));
                    }
                    if journal_failed {
                        // A broken journal cannot create an injection/completion feedback loop.
                        for action in self.ledger.release_all() {
                            if let Action::Release(item) = action {
                                self.physical.detach(self.owner(), item);
                                self.unconfirmed.insert(item, self.generation(item));
                                self.release_retry.insert(
                                    item,
                                    (self.now.saturating_add(RETRY_INTERVAL), InjectId(0)),
                                );
                            }
                        }
                        self.end_session(Some(EndReason::Released), None, out);
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosspane_input::journal::MemoryJournal;
    use crosspane_types::hid::HidUsage;
    use crosspane_types::input::LockKeys;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct FailedDownJournal {
        memory: MemoryJournal,
        fail_down: Arc<AtomicBool>,
    }

    impl Journal for FailedDownJournal {
        fn record_down(&mut self, item: Held) -> Result<(), JournalError> {
            if self.fail_down.load(Ordering::Relaxed) {
                return Err(std::io::Error::other("test down failure").into());
            }
            self.memory.record_down(item)
        }

        fn record_up(&mut self, item: Held) -> Result<(), JournalError> {
            self.memory.record_up(item)
        }

        fn held(&self) -> Result<Vec<Held>, JournalError> {
            self.memory.held()
        }
    }

    fn handle(target: &mut TargetE1, input: Input, now: MonoTime) -> Vec<Output> {
        let mut out = Vec::new();
        target.handle(&input, now, &mut out);
        out
    }

    fn start(peer: NodeId, session: SessionId) -> Input {
        Input::Link(LinkEvent::Control {
            peer,
            msg: ControlMessage::StartControl {
                session,
                entry_display: DisplayId(1),
                entry: PointDevice::new(50.0, 60.0),
                lock_keys: LockKeys::default(),
            },
        })
    }

    fn press(peer: NodeId, session: SessionId, item: Held) -> Input {
        let msg = match item {
            Held::Key(usage) => InputMessage::Key {
                session,
                seq: 1,
                usage,
                down: true,
            },
            Held::Button(button) => InputMessage::Button {
                session,
                seq: 1,
                button,
                down: true,
            },
        };
        Input::Link(LinkEvent::Input { peer, msg })
    }

    fn end(peer: NodeId, session: SessionId) -> Input {
        Input::Link(LinkEvent::Control {
            peer,
            msg: ControlMessage::EndControl {
                session,
                reason: EndReason::Released,
            },
        })
    }

    fn item_injections(out: &[Output]) -> Vec<InjectId> {
        out.iter()
            .filter_map(|output| match output {
                Output::Inject {
                    id,
                    cmd: InjectCmd::Key { .. } | InjectCmd::Button { .. },
                } => Some(*id),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn n1e2a_repeated_failed_restarts_bound_shared_release_payload() {
        let peer = NodeId([1; 32]);
        let now = MonoTime::from_nanos(1_000_000);
        for item in [
            Held::Key(HidUsage::keyboard(4)),
            Held::Button(MouseButton::PRIMARY),
        ] {
            let fail_down = Arc::new(AtomicBool::new(false));
            let journal = FailedDownJournal {
                memory: MemoryJournal::default(),
                fail_down: fail_down.clone(),
            };
            let config = EngineConfig::new(NodeId([0; 32]));
            let (mut target, out) =
                TargetE1::new(&config, Box::new(journal), MonoTime::ZERO).unwrap();
            assert!(out.is_empty());
            handle(
                &mut target,
                Input::Grants([(peer, BTreeSet::from([Capability::InputAccept]))].into()),
                MonoTime::ZERO,
            );
            handle(
                &mut target,
                Input::Session(SessionEvent::State(SessionState {
                    lock: LockState::Unlocked,
                    active: Some(true),
                })),
                MonoTime::ZERO,
            );
            handle(&mut target, start(peer, SessionId(1)), MonoTime::ZERO);
            let down = handle(&mut target, press(peer, SessionId(1), item), MonoTime::ZERO);
            let down_ids = item_injections(&down);
            assert_eq!(down_ids.len(), 1);
            handle(
                &mut target,
                Input::InjectDone {
                    id: down_ids[0],
                    ok: true,
                },
                MonoTime::ZERO,
            );
            let up = handle(&mut target, end(peer, SessionId(1)), now);
            let up_ids = item_injections(&up);
            assert_eq!(up_ids.len(), 1);
            let release = up_ids[0];
            let generation = target.unconfirmed[&item];
            fail_down.store(true, Ordering::Relaxed);
            for attempt in 0..1_000 {
                let session = SessionId(100 + attempt);
                handle(&mut target, start(peer, session), now);
                assert!(target.is_controlled());
                let refused = handle(&mut target, press(peer, session, item), now);
                assert!(item_injections(&refused).is_empty());
                assert!(!target.is_controlled());
            }
            assert_eq!(target.pending.len(), 1);
            let Pending::Release(items) = &target.pending[&release] else {
                panic!("expected shared release");
            };
            assert_eq!(items.len(), 1);
            assert_eq!(items.as_slice(), &[(item, generation)]);
            assert_eq!(target.journal.held().unwrap(), vec![item]);
            assert_eq!(
                target.journal.0.lock().unwrap().0.held().unwrap(),
                vec![item]
            );
            handle(
                &mut target,
                Input::InjectDone {
                    id: release,
                    ok: true,
                },
                now,
            );
            assert!(target.pending.is_empty());
            assert!(target.journal.held().unwrap().is_empty());
            fail_down.store(false, Ordering::Relaxed);
            handle(&mut target, start(peer, SessionId(2_000)), now);
            let fresh = handle(&mut target, press(peer, SessionId(2_000), item), now);
            assert_eq!(item_injections(&fresh).len(), 1);
            assert_ne!(target.generations[&item], generation);
            handle(
                &mut target,
                Input::InjectDone {
                    id: release,
                    ok: true,
                },
                now,
            );
            assert_eq!(target.journal.held().unwrap(), vec![item]);
            let final_up = handle(&mut target, end(peer, SessionId(2_000)), now);
            let final_ids = item_injections(&final_up);
            assert_eq!(final_ids.len(), 1);
            handle(
                &mut target,
                Input::InjectDone {
                    id: final_ids[0],
                    ok: true,
                },
                now,
            );
            assert!(target.pending.is_empty());
            assert!(target.journal.held().unwrap().is_empty());
        }
    }

    fn drag_fixture() -> (TargetE1, NodeId, ProjectionKey) {
        let peer = NodeId([1; 32]);
        let config = EngineConfig::new(NodeId([0; 32]));
        let (mut target, out) =
            TargetE1::new(&config, Box::<MemoryJournal>::default(), MonoTime::ZERO).unwrap();
        assert!(out.is_empty());
        handle(
            &mut target,
            Input::Grants([(peer, BTreeSet::from([Capability::InputAccept]))].into()),
            MonoTime::ZERO,
        );
        handle(
            &mut target,
            Input::Session(SessionEvent::State(SessionState {
                lock: LockState::Unlocked,
                active: Some(true),
            })),
            MonoTime::ZERO,
        );
        handle(&mut target, start(peer, SessionId(1)), MonoTime::ZERO);
        let key = ProjectionKey {
            source: peer,
            projection: crosspane_types::id::ProjectionId(1),
        };
        target.prepare_drag(
            key,
            7,
            DisplayId(1),
            PointDevice::new(50.0, 60.0),
            &mut Vec::new(),
        );
        (target, peer, key)
    }

    fn press_at(peer: NodeId) -> Input {
        Input::Link(LinkEvent::Input {
            peer,
            msg: InputMessage::PressAt {
                session: SessionId(1),
                seq: 2,
                button: MouseButton::PRIMARY,
                display: DisplayId(1),
                position: PointDevice::new(50.0, 60.0),
            },
        })
    }

    fn item_message(peer: NodeId, item: Held, seq: u32, down: bool) -> Input {
        let msg = match item {
            Held::Key(usage) => InputMessage::Key {
                session: SessionId(1),
                seq,
                usage,
                down,
            },
            Held::Button(button) => InputMessage::Button {
                session: SessionId(1),
                seq,
                button,
                down,
            },
        };
        Input::Link(LinkEvent::Input { peer, msg })
    }

    fn downs(out: &[Output], item: Held) -> usize {
        out.iter()
            .filter(|o| match (o, item) {
                (
                    Output::Inject {
                        cmd: InjectCmd::Key { usage, down: true },
                        ..
                    },
                    Held::Key(key),
                ) => *usage == key,
                (
                    Output::Inject {
                        cmd: InjectCmd::Button { button, down: true },
                        ..
                    },
                    Held::Button(wanted),
                ) => *button == wanted,
                _ => false,
            })
            .count()
    }

    #[test]
    fn n1e2b_stale_drag_failure_preserves_fresh_primary_and_other_holds() {
        for cleanup in [true, false] {
            let (mut target, peer, key) = drag_fixture();
            let dragging = complete_drag(&mut target, peer, key);
            let old_down = item_injections(&dragging)[0];
            let primary = Held::Button(MouseButton::PRIMARY);
            let released = if cleanup {
                let mut out = Vec::new();
                target.cleanup_item(primary, MonoTime::ZERO, &mut out);
                out
            } else {
                handle(
                    &mut target,
                    item_message(peer, primary, 3, false),
                    MonoTime::ZERO,
                )
            };
            assert_eq!(item_injections(&released).len(), 1);
            for id in item_injections(&released) {
                handle(
                    &mut target,
                    Input::InjectDone { id, ok: true },
                    MonoTime::ZERO,
                );
            }
            assert!(target.journal.held().unwrap().is_empty());
            // Current uncertain continuation cleanup may end control conservatively. A new
            // session is admitted normally; a released continuation can retain its session.
            if target.controller().is_none() {
                handle(&mut target, start(peer, SessionId(1)), MonoTime::ZERO);
            }
            let other = Held::Key(HidUsage::keyboard(5));
            for (seq, item) in [(4, primary), (5, other)] {
                let out = handle(
                    &mut target,
                    item_message(peer, item, seq, true),
                    MonoTime::ZERO,
                );
                assert_eq!(downs(&out, item), 1);
                let id = item_injections(&out)[0];
                assert_ne!(id, old_down);
                handle(
                    &mut target,
                    Input::InjectDone { id, ok: true },
                    MonoTime::ZERO,
                );
            }
            let before = target.journal.held().unwrap();
            let stale = handle(
                &mut target,
                Input::InjectDone {
                    id: old_down,
                    ok: false,
                },
                MonoTime::ZERO,
            );
            assert!(item_injections(&stale).is_empty(), "{stale:?}");
            assert_eq!(target.controller(), Some(peer));
            assert_eq!(target.journal.held().unwrap(), before);
            assert_eq!(target.ledger.held(), before);
            let ended = handle(&mut target, end(peer, SessionId(1)), MonoTime::ZERO);
            assert_eq!(item_injections(&ended).len(), 2);
            for id in item_injections(&ended) {
                handle(
                    &mut target,
                    Input::InjectDone { id, ok: true },
                    MonoTime::ZERO,
                );
            }
            assert!(target.journal.held().unwrap().is_empty());
            assert!(target.physical.settled());
        }
    }

    fn complete_drag(target: &mut TargetE1, peer: NodeId, key: ProjectionKey) -> Vec<Output> {
        let moving = handle(target, press_at(peer), MonoTime::ZERO);
        let id = moving
            .iter()
            .find_map(|out| match out {
                Output::Inject {
                    id,
                    cmd: InjectCmd::MoveTo { .. },
                } => Some(*id),
                _ => None,
            })
            .unwrap();
        let armed = handle(target, Input::InjectDone { id, ok: true }, MonoTime::ZERO);
        assert!(
            armed
                .iter()
                .any(|out| matches!(out, Output::ArmDrag { .. }))
        );
        handle(
            target,
            Input::DragArmed {
                key,
                token: 7,
                ok: true,
            },
            MonoTime::ZERO,
        )
    }

    #[test]
    fn n1e2b_absorbed_drag_down_cannot_swallow_real_primary_up_completion() {
        let (mut target, peer, key) = drag_fixture();
        let primary = Held::Button(MouseButton::PRIMARY);
        let pressed = handle(
            &mut target,
            item_message(peer, primary, 1, true),
            MonoTime::ZERO,
        );
        let down = item_injections(&pressed)[0];
        handle(
            &mut target,
            Input::InjectDone { id: down, ok: true },
            MonoTime::ZERO,
        );
        let continued = complete_drag(&mut target, peer, key);
        assert_eq!(downs(&continued, primary), 0);
        assert!(item_injections(&continued).is_empty());
        let released = handle(
            &mut target,
            item_message(peer, primary, 3, false),
            MonoTime::ZERO,
        );
        assert_eq!(item_injections(&released).len(), 1);
        let up = item_injections(&released)[0];
        assert_ne!(up, down);
        let confirmed = handle(
            &mut target,
            Input::InjectDone { id: up, ok: true },
            MonoTime::ZERO,
        );
        assert!(item_injections(&confirmed).is_empty());
        assert!(target.journal.held().unwrap().is_empty());
        assert!(target.unconfirmed.is_empty() && target.pending.is_empty());
        assert!(target.physical.settled());
        let tick = handle(&mut target, Input::Tick, MonoTime::from_nanos(50_000_000));
        assert!(
            item_injections(&tick).is_empty(),
            "no duplicate Up: {tick:?}"
        );
        let ended = handle(
            &mut target,
            end(peer, SessionId(1)),
            MonoTime::from_nanos(50_000_000),
        );
        assert!(item_injections(&ended).is_empty());
    }

    #[test]
    fn n1e2b_drag_refuses_other_role_primary_at_each_activation_boundary() {
        for boundary in 0..3 {
            let (mut target, peer, key) = drag_fixture();
            let item = Held::Button(MouseButton::PRIMARY);
            let other = Owner::E2(crosspane_types::id::ProjectionId(99));
            let mut other_journal = MemoryJournal::default();
            let own = |target: &TargetE1, journal: &mut MemoryJournal| {
                journal.record_down(item).unwrap();
                assert!(
                    target
                        .physical
                        .press(other, item, InjectId(9_000), None)
                        .unwrap()
                );
            };
            if boundary == 0 {
                own(&target, &mut other_journal);
            }
            let mut out = handle(&mut target, press_at(peer), MonoTime::ZERO);
            if boundary > 0 {
                let move_id = out
                    .iter()
                    .find_map(|o| match o {
                        Output::Inject {
                            id,
                            cmd: InjectCmd::MoveTo { .. },
                        } => Some(*id),
                        _ => None,
                    })
                    .unwrap();
                if boundary == 1 {
                    own(&target, &mut other_journal);
                }
                out = handle(
                    &mut target,
                    Input::InjectDone {
                        id: move_id,
                        ok: true,
                    },
                    MonoTime::ZERO,
                );
                if boundary == 2 {
                    assert!(out.iter().any(|o| matches!(o, Output::ArmDrag { .. })));
                    own(&target, &mut other_journal);
                    out = handle(
                        &mut target,
                        Input::DragArmed {
                            key,
                            token: 7,
                            ok: true,
                        },
                        MonoTime::ZERO,
                    );
                }
            }
            assert_eq!(downs(&out, item), 0);
            assert!(!out.iter().any(|o| matches!(o, Output::ArmDrag { .. })));
            assert_eq!(target.controller(), None);
            assert!(target.ledger.held().is_empty());
            assert_eq!(other_journal.held().unwrap(), vec![item]);
            assert!(matches!(
                target
                    .physical
                    .release(Some(other), item, InjectId(9_001), MonoTime::ZERO)
                    .unwrap(),
                ReleaseAction::Submit { emit: true, .. }
            ));
        }
    }

    #[test]
    fn n1e2b_drag_queue_drops_blocked_and_previously_queued_item_downs() {
        for item in [
            Held::Key(HidUsage::keyboard(4)),
            Held::Button(MouseButton(2)),
        ] {
            for queued_before_cleanup in [true, false] {
                for move_before_cleanup_confirmation in [true, false] {
                    let (mut target, peer, key) = drag_fixture();
                    let down = handle(
                        &mut target,
                        item_message(peer, item, 1, true),
                        MonoTime::ZERO,
                    );
                    let down_id = item_injections(&down)[0];
                    handle(
                        &mut target,
                        Input::InjectDone {
                            id: down_id,
                            ok: true,
                        },
                        MonoTime::ZERO,
                    );
                    let moving = handle(&mut target, press_at(peer), MonoTime::ZERO);
                    let move_id = moving
                        .iter()
                        .find_map(|o| match o {
                            Output::Inject {
                                id,
                                cmd: InjectCmd::MoveTo { .. },
                            } => Some(*id),
                            _ => None,
                        })
                        .unwrap();
                    if queued_before_cleanup {
                        handle(
                            &mut target,
                            item_message(peer, item, 3, true),
                            MonoTime::ZERO,
                        );
                    }
                    let mut cleanup = Vec::new();
                    target.cleanup_item(item, MonoTime::ZERO, &mut cleanup);
                    let up_id = item_injections(&cleanup)[0];
                    if !queued_before_cleanup {
                        handle(
                            &mut target,
                            item_message(peer, item, 3, true),
                            MonoTime::ZERO,
                        );
                    }
                    assert!(target.drag_press.as_ref().unwrap().queue.is_empty());
                    if move_before_cleanup_confirmation {
                        handle(
                            &mut target,
                            Input::InjectDone {
                                id: move_id,
                                ok: true,
                            },
                            MonoTime::ZERO,
                        );
                    }
                    handle(
                        &mut target,
                        Input::InjectDone {
                            id: up_id,
                            ok: true,
                        },
                        MonoTime::ZERO,
                    );
                    if !move_before_cleanup_confirmation {
                        handle(
                            &mut target,
                            Input::InjectDone {
                                id: move_id,
                                ok: true,
                            },
                            MonoTime::ZERO,
                        );
                    }
                    let flushed = handle(
                        &mut target,
                        Input::DragArmed {
                            key,
                            token: 7,
                            ok: true,
                        },
                        MonoTime::ZERO,
                    );
                    assert_eq!(downs(&flushed, item), 0);
                    assert!(!target.ledger.held().contains(&item));
                    let fresh = handle(
                        &mut target,
                        item_message(peer, item, 4, true),
                        MonoTime::ZERO,
                    );
                    assert_eq!(downs(&fresh, item), 1);
                    handle(
                        &mut target,
                        item_message(peer, item, 5, false),
                        MonoTime::ZERO,
                    );
                    handle(&mut target, end(peer, SessionId(1)), MonoTime::ZERO);
                }
            }
        }
    }
}
