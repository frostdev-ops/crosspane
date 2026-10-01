//! OS permissions and onboarding state (04 §7, WP-1.20).

use std::sync::Arc;

use crate::{EventSink, Permission, PlatformError};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PermissionState {
    Granted,
    NotGranted,
    Unknown,
}

/// Read-only permission status for onboarding, without trying the operation the permission guards.
pub trait Permissions: Send {
    /// The permissions this platform needs (empty where none apply, e.g. on Hyprland).
    fn required(&self) -> Vec<Permission>;

    /// The current status, from the OS's preflight APIs.
    fn state(&self, permission: Permission) -> PermissionState;

    /// Start the OS's own flow for granting `permission` (a system prompt or the settings pane).
    /// Success means the flow started, not that the user granted anything.
    fn request(&mut self, permission: Permission) -> Result<(), PlatformError>;

    /// Deliver the status of every required permission now, then each change (including
    /// revocation) as it is observed.
    fn subscribe(
        &mut self,
        sink: Arc<dyn EventSink<(Permission, PermissionState)>>,
    ) -> Result<(), PlatformError>;
}
