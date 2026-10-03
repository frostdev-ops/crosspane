//! The target role: acceptance, journaled injection, leases and local override.

use core::fmt;
use core::time::Duration;
use std::collections::{BTreeMap, BTreeSet};

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
use crosspane_types::id::{NodeId, SessionId};
use crosspane_types::time::MonoTime;

use crate::config::EngineConfig;
use crate::io::{Command, InjectCmd, InjectId, Input, Notice, Output, TARGET_INDICATOR};

const RETRY_INTERVAL: Duration = Duration::from_millis(50);

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

/// The target side of E1: accepting control, injecting through the lease ledger, local override.
pub struct TargetE1 {
    state: SessionState,
    asleep: bool,
    awaiting_state: bool,
    grants: BTreeMap<NodeId, BTreeSet<Capability>>,
    ledger: TargetLedger<Box<dyn Journal>>,
    active: Option<ActiveSession>,
    pending: BTreeMap<InjectId, Pending>,
    next_id: u64,
    now: MonoTime,
    recovery: Vec<Held>,
    recovery_failed: bool,
    // A completion for an earlier release must never clear a later press's journal record.
    generations: BTreeMap<Held, u64>,
    unconfirmed: BTreeMap<Held, u64>,
    release_retry: BTreeSet<Held>,
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
        let (ledger, recovery) = TargetLedger::open(journal)?;
        let mut target = Self {
            state: SessionState {
                lock: LockState::Locked,
                active: None,
            },
            asleep: false,
            awaiting_state: true,
            grants: BTreeMap::new(),
            ledger,
            active: None,
            pending: BTreeMap::new(),
            next_id: 1,
            now,
            recovery,
            recovery_failed: false,
            generations: BTreeMap::new(),
            unconfirmed: BTreeMap::new(),
            release_retry: BTreeSet::new(),
        };
        let mut out = Vec::new();
        target.recover(&mut out);
        Ok((target, out))
    }

    /// Handle one input (every input is offered to both roles), appending outputs.
    pub fn handle(&mut self, input: &Input, now: MonoTime, out: &mut Vec<Output>) {
        self.now = now;
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
        let retry = if !self.recovery.is_empty() || !self.release_retry.is_empty() {
            Some(self.now.saturating_add(RETRY_INTERVAL))
        } else {
            None
        };
        [self.ledger.next_deadline(), retry]
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
        let (item, down, pending) = match action {
            Action::Press(item) => {
                self.generations.insert(item, self.next_id);
                self.unconfirmed.remove(&item);
                self.release_retry.remove(&item);
                (item, true, None)
            }
            Action::Release(item) => {
                let generation = self.generation(item);
                self.unconfirmed.insert(item, generation);
                (
                    item,
                    false,
                    Some(Pending::Release(vec![(item, generation)])),
                )
            }
        };
        let cmd = match item {
            Held::Key(usage) => InjectCmd::Key { usage, down },
            Held::Button(button) => InjectCmd::Button { button, down },
        };
        self.inject(cmd, pending, out);
    }

    fn generation(&self, item: Held) -> u64 {
        self.generations.get(&item).copied().unwrap_or(0)
    }

    fn release_all(&mut self, force: bool, out: &mut Vec<Output>) {
        let actions = self.ledger.release_all();
        if actions.is_empty() && !force {
            return;
        }
        for action in actions {
            if let Action::Release(item) = action {
                self.unconfirmed.insert(item, self.generation(item));
            }
        }
        let items = self.unconfirmed.iter().map(|(&k, &v)| (k, v)).collect();
        self.inject(InjectCmd::ReleaseAll, Some(Pending::Release(items)), out);
    }

    fn end_session(
        &mut self,
        reason: Option<EndReason>,
        notice: Option<Notice>,
        out: &mut Vec<Output>,
    ) {
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
        // Even a failed record_down may have partially updated the journal. Never press it.
        self.release_all(true, out);
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

    fn key_or_button(&mut self, item: Held, down: bool, out: &mut Vec<Output>) {
        if down && !self.permits_io() {
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
        if self.recovery_failed {
            self.recover(out);
        }
        if !self.release_retry.is_empty() {
            // release_all also releases newly held items; journal those as pending releases too.
            self.release_all(true, out);
        }
    }

    fn recover(&mut self, out: &mut Vec<Output>) {
        if self.recovery.is_empty() {
            return;
        }
        let mut keys = Vec::new();
        let mut buttons = Vec::new();
        for item in &self.recovery {
            match item {
                Held::Key(key) => keys.push(*key),
                Held::Button(button) => buttons.push(*button),
            }
        }
        self.inject(
            InjectCmd::Recover { keys, buttons },
            Some(Pending::Recover(self.recovery.clone())),
            out,
        );
    }

    fn inject_done(&mut self, id: InjectId, ok: bool, out: &mut Vec<Output>) {
        match self.pending.remove(&id) {
            Some(Pending::Recover(items)) if !self.recovery.is_empty() => {
                let items: Vec<_> = items
                    .into_iter()
                    .filter(|&item| self.generation(item) == 0)
                    .collect();
                if ok && self.ledger.recovered(&items).is_ok() {
                    self.recovery.clear();
                    self.recovery_failed = false;
                } else {
                    self.recovery_failed = true;
                }
            }
            Some(Pending::Release(items)) => {
                let items: Vec<_> = items
                    .into_iter()
                    .filter(|(item, generation)| self.unconfirmed.get(item) == Some(generation))
                    .map(|(item, _)| item)
                    .collect();
                if ok {
                    if self.ledger.confirm_released(&items).is_ok() {
                        for item in items {
                            self.unconfirmed.remove(&item);
                            self.release_retry.remove(&item);
                        }
                    } else {
                        self.release_retry.extend(items);
                        // Defer remaining held items too, so ending the session cannot emit
                        // another injection from this completion. Only a Tick retries releases.
                        for action in self.ledger.release_all() {
                            if let Action::Release(item) = action {
                                self.unconfirmed.insert(item, self.generation(item));
                                self.release_retry.insert(item);
                            }
                        }
                        self.end_session(Some(EndReason::Released), None, out);
                    }
                } else {
                    self.release_retry.extend(items);
                }
            }
            _ => {}
        }
    }
}
