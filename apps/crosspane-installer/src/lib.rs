//! Crosspane's display-only installer shell. No production ports are constructed here.

pub mod agent_contract;
pub mod demo;
pub mod gui;
pub mod motion;
mod screens;
pub mod view;

pub use motion::{reduced_motion, transition_fraction};
pub use screens::WizardShell;
pub use view::*;
