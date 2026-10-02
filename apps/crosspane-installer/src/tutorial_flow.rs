//! Pure, attempt-bound practice orchestration. Consumers must reduce every core effect in
//! order, stop on core errors, and report them through `TutorialEvent::CoreRejected`.
//! Before a practice action, require core's matching Verify job after Observe, including
//! prerequisite expiry. Known-owned cleanup/detection may continue after invalidation.
mod messages;
mod sequencing;
mod settings;

pub use messages::*;
pub use sequencing::Tutorial;
pub use settings::{SettingsOutcome, SettingsTransition, SettingsTransitionState};
