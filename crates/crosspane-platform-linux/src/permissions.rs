//! Linux needs no OS permission grants for Crosspane's Hyprland backends (02 §3.3).

use std::sync::Arc;

use crosspane_platform::{EventSink, Permission, PermissionState, Permissions, PlatformError};

/// `Permissions` for Linux: nothing is required.
#[derive(Debug, Default)]
pub struct LinuxPermissions;

impl Permissions for LinuxPermissions {
    fn required(&self) -> Vec<Permission> {
        Vec::new()
    }

    fn state(&self, _permission: Permission) -> PermissionState {
        PermissionState::Granted
    }

    fn request(&mut self, _permission: Permission) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported("no permission flow on Linux"))
    }

    fn subscribe(
        &mut self,
        _sink: Arc<dyn EventSink<(Permission, PermissionState)>>,
    ) -> Result<(), PlatformError> {
        Ok(())
    }
}
