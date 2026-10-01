//! The controller role. Implemented in WP-1.22a.

use crosspane_types::time::MonoTime;

use crate::config::EngineConfig;
use crate::io::{Input, Output};

/// The controller side of E1: crossing, capture, routing, heartbeats, release and panic.
#[derive(Debug)]
pub struct ControllerE1 {
    _private: (),
}

impl ControllerE1 {
    pub fn new(config: &EngineConfig, now: MonoTime) -> ControllerE1 {
        let _ = (config, now);
        ControllerE1 { _private: () }
    }

    /// Handle one input (every input is offered to both roles), appending outputs.
    pub fn handle(&mut self, input: &Input, now: MonoTime, out: &mut Vec<Output>) {
        let _ = (input, now, out);
    }

    pub fn next_deadline(&self) -> Option<MonoTime> {
        None
    }
}
