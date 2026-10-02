//! Orchestration, OS-free (03 §1): E1 sessions now; projection lifecycle, focus guard and parking
//! recovery in Phase 2. Depends only on traits and logical messages.
//!
//! The engine is **sans-IO**: [`Engine::handle`] takes one [`Input`] and returns the [`Output`]s to
//! carry out. The agent executes outputs against the real platform traits and peer links;
//! `crosspane-testkit` executes them against fakes and a simulated network. All time comes from
//! the `now` argument, so runs are deterministic.

#![deny(unsafe_code)]

mod audio;
pub mod config;
pub mod e1;
pub mod e2;
pub mod io;

use crosspane_input::journal::{Journal, JournalError};
use crosspane_platform::CaptureEvent;
use crosspane_protocol::link::LinkEvent;
use crosspane_protocol::msg::{ControlMessage, InputMessage, Refusal};
use crosspane_protocol::projection::ProjInput;
use crosspane_types::id::NodeId;
use crosspane_types::time::MonoTime;

pub use config::EngineConfig;
pub use io::{
    Command, Failure, InjectCmd, InjectId, Input, Notice, Output, ProjectionKey, ProxyEvent,
};

use e1::controller::ControllerE1;
use e1::target::TargetE1;
use e2::E2;

/// One node's engine: the E1 controller and target roles and the E2 roles side by side.
#[derive(Debug)]
pub struct Engine {
    controller: ControllerE1,
    target: TargetE1,
    e2: E2,
    audio: audio::Audio,
}

impl Engine {
    /// Start the engine. The returned outputs include crash recovery from both journals (04 §8
    /// invariant 2): `journal` for E1 injection, `e2_journal` for input injected into projected
    /// windows. The agent runs them before anything else.
    pub fn new(
        config: EngineConfig,
        journal: Box<dyn Journal>,
        e2_journal: Box<dyn Journal>,
        now: MonoTime,
    ) -> Result<(Engine, Vec<Output>), JournalError> {
        let (target, mut out) = TargetE1::new(&config, journal, now)?;
        let (e2, e2_out) = E2::new(&config, e2_journal, now)?;
        out.extend(e2_out);
        // The engine permits I/O from the start (after recovery); only a panic closes its side.
        out.push(Output::EngineGate(true));
        let controller = ControllerE1::new(&config, now);
        Ok((
            Engine {
                controller,
                target,
                e2,
                audio: audio::Audio::new(config.node),
            },
            out,
        ))
    }

    /// Handle one input. Panic (04 §6) closes the engine side of the I/O gate before either role
    /// reacts; re-arming opens it.
    pub fn handle(&mut self, input: Input, now: MonoTime) -> Vec<Output> {
        let mut out = Vec::new();
        match &input {
            Input::Command(Command::Panic) => out.push(Output::EngineGate(false)),
            Input::Command(Command::Rearm) => out.push(Output::EngineGate(true)),
            _ => {}
        }
        // While another node controls this one, this node's own portals are inert: the injected
        // pointer enters at an edge, and crossing out from there would bounce control straight
        // back (found in the nested end-to-end test).
        let incoming_start = if let Input::Link(LinkEvent::Control {
            peer,
            msg: ControlMessage::StartControl { session, .. },
        }) = &input
        {
            Some((*peer, *session))
        } else {
            None
        };
        let refuse_start = incoming_start.is_some() && self.controller.started();
        if let Some((peer, session)) = incoming_start {
            if refuse_start {
                out.push(Output::SendControl {
                    peer,
                    msg: ControlMessage::ControlRefused {
                        session,
                        reason: Refusal::Busy,
                    },
                });
                out.push(Output::Notice(Notice::Refused {
                    peer,
                    reason: Refusal::Busy,
                }));
            } else if self.target.would_admit(peer) {
                // Only a request the target will admit may pre-empt this node's own pending
                // crossing: a peer without the input grant (or a locked node) can't cancel it.
                self.controller.cancel_pending(now, &mut out);
            }
        }
        let was_controlled = self.target.is_controlled();
        let edge_event = matches!(
            &input,
            Input::Capture(CaptureEvent::EdgePressed { .. } | CaptureEvent::EdgeReleased { .. })
        );
        // WP-2.43 §2.10: a peer's motion over a proxy of this node's twin-parked window may take
        // this node's input home. What E2 would accept is decided first (pure); the controller
        // corroborates it against its own pointer model.
        let motion = match &input {
            Input::Link(LinkEvent::Input {
                peer,
                msg: InputMessage::Proj(msg @ ProjInput::Motion { .. }),
            }) => self
                .e2
                .prevalidate_motion(*peer, msg)
                .map(|(projection, at)| (*peer, projection, at)),
            _ => None,
        };
        if !(was_controlled && edge_event) {
            self.controller.handle(&input, now, &mut out);
        }
        if let Some((peer, projection, position)) = motion {
            self.controller
                .peer_motion(peer, projection, position, now, &mut out);
        }
        if !refuse_start {
            self.target.handle(&input, now, &mut out);
        }
        // Home is decided before E2 sees the input that triggered it (so it is never injected).
        // E2 drains and filters; the controller advances the entry only once every injector this
        // node owns has settled, after E2's outputs; E2's changed twin set reaches the controller
        // last, and the trailing set_home lifts the filter in the same handle as an abort or exit.
        self.e2.set_home(self.controller.home(), now, &mut out);
        self.e2.handle(&input, now, &mut out);
        // (Only an entry that is waiting for the drain looks at it, so it is only asked then.)
        let settled = self.controller.draining() && self.e2.settled() && self.target.settled();
        self.controller.after_e2(settled, now, &mut out);
        self.controller
            .set_twin_homes(self.e2.twin_homes(), now, &mut out);
        self.e2.set_home(self.controller.home(), now, &mut out);
        let audio_gates: Vec<_> = out
            .iter()
            .filter_map(|output| match output {
                Output::EngineGate(permits) => Some(*permits),
                _ => None,
            })
            .collect();
        for permits in audio_gates {
            self.audio.engine_gate(permits, &mut out);
        }
        self.audio.handle(&input, now, &mut out);
        let controlled = self.target.is_controlled();
        if controlled {
            out.retain(|o| !matches!(o, Output::SetPortals(_)));
            if !was_controlled {
                out.push(Output::SetPortals(Vec::new()));
            }
        } else if was_controlled {
            // The pointer is still where the controller left it, often at the entry edge: the
            // restored portals must not turn that into a crossing of this node's own.
            self.controller.portals_restored(now);
            out.push(Output::SetPortals(self.controller.portals().to_vec()));
        }
        // Each `SetPortals` actually emitted (the final list: the controller's own that were
        // suppressed above are not in it) is answered by one `Input::PortalsSet`, in order, and
        // registers the mapping it was emitted under (WP-2.43 B1).
        for output in &out {
            if let Output::SetPortals(set) = output {
                self.controller.portal_emitted(set);
            }
        }
        out
    }

    /// The node this node's keyboard and mouse drive right now (E1 controller), if any.
    pub fn controlling(&self) -> Option<NodeId> {
        self.controller.target()
    }

    /// The node driving this one (E1 target), if any.
    pub fn controlled_by(&self) -> Option<NodeId> {
        self.target.controller()
    }

    /// Whether edge crossing is armed (false after a panic or release until re-armed).
    pub fn armed(&self) -> bool {
        self.controller.armed()
    }

    /// When the agent must deliver the next [`Input::Tick`].
    pub fn next_deadline(&self) -> Option<MonoTime> {
        [
            self.controller.next_deadline(),
            self.target.next_deadline(),
            self.e2.next_deadline(),
            self.audio.next_deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
    }
}
