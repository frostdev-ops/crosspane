//! The target role. Implemented in WP-1.22b.

use crosspane_input::journal::{Journal, JournalError};
use crosspane_types::time::MonoTime;

use crate::config::EngineConfig;
use crate::io::{Input, Output};

/// The target side of E1: accepting control, injecting through the lease ledger, local override.
pub struct TargetE1 {
    _private: (),
}

impl TargetE1 {
    /// Opens the journal; the returned outputs release whatever a crashed previous process left
    /// held (04 §8 invariant 2).
    pub fn new(
        config: &EngineConfig,
        journal: Box<dyn Journal>,
        now: MonoTime,
    ) -> Result<(TargetE1, Vec<Output>), JournalError> {
        let _ = (config, journal, now);
        Ok((TargetE1 { _private: () }, Vec::new()))
    }

    /// Handle one input (every input is offered to both roles), appending outputs.
    pub fn handle(&mut self, input: &Input, now: MonoTime, out: &mut Vec<Output>) {
        let _ = (input, now, out);
    }

    pub fn next_deadline(&self) -> Option<MonoTime> {
        None
    }
}
