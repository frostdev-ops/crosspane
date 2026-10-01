//! E2 window projection roles (docs/wp/E2-v0.md): the source side (the window's owner) and the
//! destination side (the proxy), side by side. Implemented in WP-2.5; the API is frozen.

use crosspane_input::journal::{Journal, JournalError};
use crosspane_types::time::MonoTime;

use crate::config::EngineConfig;
use crate::io::{Input, Output};

/// Both E2 roles of one node.
#[derive(Debug)]
pub struct E2 {
    _private: (),
}

impl E2 {
    /// Start the E2 roles. `journal` records keys and buttons injected into projected windows
    /// (separate from E1's); the returned outputs are its crash recovery, run first.
    pub fn new(
        _config: &EngineConfig,
        _journal: Box<dyn Journal>,
        _now: MonoTime,
    ) -> Result<(E2, Vec<Output>), JournalError> {
        Ok((E2 { _private: () }, Vec::new()))
    }

    pub fn handle(&mut self, _input: &Input, _now: MonoTime, _out: &mut Vec<Output>) {}

    pub fn next_deadline(&self) -> Option<MonoTime> {
        None
    }
}
