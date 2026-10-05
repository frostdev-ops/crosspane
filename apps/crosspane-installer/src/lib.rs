//! Crosspane's display-only installer shell. No production ports are constructed here.

pub mod agent_contract;
pub mod demo;
pub mod fixture;
pub mod gui;
pub mod live;
pub mod motion;
pub mod platform;
pub mod review;
mod screens;
pub mod tutorial_flow;
pub mod tutorial_window;
pub mod view;

pub use motion::{MotionLevel, motion_level, reduced_motion, transition_fraction};
pub use screens::WizardShell;
pub use view::*;

pub mod diagnose;
