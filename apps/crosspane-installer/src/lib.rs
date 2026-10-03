//! Crosspane's display-only installer shell. No production ports are constructed here.

pub mod agent_contract;
pub mod demo;
pub mod fixture;
pub mod gui;
pub mod motion;
pub mod platform;
mod screens;
pub mod tutorial_flow;
pub mod tutorial_window;
pub mod view;

pub use motion::{reduced_motion, transition_fraction};
pub use screens::WizardShell;
pub use view::*;
