//! Inert ordinary-user Mac adapters. Construction never selects an owner's environment.
pub mod audio_package;
pub mod fonts;
pub mod integration;
pub mod launch_agent;
mod launchd_observation;
pub mod native_io;
pub mod payload;
pub mod permissions;
pub mod removal;
pub mod repair;
pub mod transport;
pub mod tutorial;
