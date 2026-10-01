//! Orchestration, OS-free (03 §1): E1 sessions now; projection lifecycle, focus guard and parking
//! recovery in Phase 2. Depends only on traits and logical messages.
//!
//! The engine is **sans-IO**: [`Engine::handle`] takes one [`Input`] and returns the [`Output`]s to
//! carry out. The agent executes outputs against the real platform traits and peer links;
//! `crosspane-testkit` executes them against fakes and a simulated network. All time comes from
//! the `now` argument, so runs are deterministic.

#![deny(unsafe_code)]

pub mod config;
pub mod e1;
pub mod e2;
pub mod io;

use crosspane_input::journal::{Journal, JournalError};
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
        let controller = ControllerE1::new(&config, now);
        Ok((
            Engine {
                controller,
                target,
                e2,
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
        self.controller.handle(&input, now, &mut out);
        self.target.handle(&input, now, &mut out);
        self.e2.handle(&input, now, &mut out);
        out
    }

    /// When the agent must deliver the next [`Input::Tick`].
    pub fn next_deadline(&self) -> Option<MonoTime> {
        [
            self.controller.next_deadline(),
            self.target.next_deadline(),
            self.e2.next_deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
    }
}
