//! Windows (v0) needs no OS permission grants: E1/E2 run in the user's session with standard
//! privileges; nothing here opens settings or prompts.

use std::sync::Arc;

use crosspane_platform::{EventSink, Permission, PermissionState, Permissions, PlatformError};

/// `Permissions` for Windows: nothing is required, so nothing is prompted or reported.
#[derive(Clone, Copy, Debug, Default)]
pub struct WindowsPermissions;

impl Permissions for WindowsPermissions {
    fn required(&self) -> Vec<Permission> {
        Vec::new()
    }

    fn state(&self, _permission: Permission) -> PermissionState {
        PermissionState::Granted
    }

    fn request(&mut self, _permission: Permission) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported("no permission flow on Windows"))
    }

    fn subscribe(
        &mut self,
        _sink: Arc<dyn EventSink<(Permission, PermissionState)>>,
    ) -> Result<(), PlatformError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn permissions_windows_requires_nothing_and_reports_granted() {
        let all = [
            Permission::ScreenRecording,
            Permission::Accessibility,
            Permission::InputMonitoring,
            Permission::Microphone,
        ];
        let mut permissions = WindowsPermissions;

        assert!(permissions.required().is_empty());
        for permission in all {
            assert_eq!(permissions.state(permission), PermissionState::Granted);
            assert!(matches!(
                permissions.request(permission),
                Err(PlatformError::Unsupported("no permission flow on Windows"))
            ));
        }

        let delivered = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&delivered);
        let sink: Arc<dyn EventSink<(Permission, PermissionState)>> =
            Arc::new(move |_event: (Permission, PermissionState)| {
                counter.fetch_add(1, Ordering::SeqCst);
            });
        assert!(permissions.subscribe(sink).is_ok());
        assert_eq!(delivered.load(Ordering::SeqCst), 0);
    }
}
