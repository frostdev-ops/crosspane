//! TCC status and onboarding (04 §7, WP-1.20) through the OS's own preflight and request APIs. Never
//! reads or edits the TCC database.
//!
//! Grants belong to the code-signing identity of the *responsible* process, so the agent must run as
//! the signed `Crosspane.app` bundle (`io.frostdev.crosspane.agent`, scripts/macos/bundle.sh), not as
//! a bare binary under a terminal or SSH.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crosspane_platform::{EventSink, Permission, PermissionState, Permissions, PlatformError};
use objc2_application_services::{
    AXIsProcessTrusted, AXIsProcessTrustedWithOptions, kAXTrustedCheckOptionPrompt,
};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFString};
use objc2_core_graphics::{
    CGPreflightListenEventAccess, CGPreflightPostEventAccess, CGPreflightScreenCaptureAccess,
    CGRequestListenEventAccess, CGRequestPostEventAccess, CGRequestScreenCaptureAccess,
};

/// How often `subscribe` re-reads the status. TCC has no change notification.
const POLL: Duration = Duration::from_secs(1);

const REQUIRED: [Permission; 3] = [
    Permission::ScreenRecording,
    Permission::Accessibility,
    Permission::InputMonitoring,
];

#[derive(Debug, Default)]
pub struct MacPermissions {
    stop: Option<Arc<AtomicBool>>,
}

impl MacPermissions {
    pub fn new() -> MacPermissions {
        MacPermissions::default()
    }

    /// The System Settings pane for `permission`, for when the one-time system prompt has already
    /// been answered and won't appear again.
    pub fn settings_url(permission: Permission) -> &'static str {
        match permission {
            Permission::ScreenRecording => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
            }
            Permission::Accessibility => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            }
            Permission::InputMonitoring => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent"
            }
            _ => "x-apple.systempreferences:com.apple.preference.security",
        }
    }
}

impl Drop for MacPermissions {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop {
            stop.store(true, Ordering::Release);
        }
    }
}

/// The current status. Screen Recording's preflight result can stay stale within one process after
/// the user changes it; the agent asks the user to relaunch after granting it.
pub fn state(permission: Permission) -> PermissionState {
    let granted = match permission {
        Permission::ScreenRecording => CGPreflightScreenCaptureAccess(),
        Permission::Accessibility => {
            // SAFETY: takes no arguments and only reads this process's trust status.
            let trusted = unsafe { AXIsProcessTrusted() };
            // Posting events needs Accessibility; both checks must agree.
            trusted && CGPreflightPostEventAccess()
        }
        Permission::InputMonitoring => CGPreflightListenEventAccess(),
        _ => return PermissionState::Unknown,
    };
    if granted {
        PermissionState::Granted
    } else {
        PermissionState::NotGranted
    }
}

impl Permissions for MacPermissions {
    fn required(&self) -> Vec<Permission> {
        REQUIRED.to_vec()
    }

    fn state(&self, permission: Permission) -> PermissionState {
        state(permission)
    }

    fn request(&mut self, permission: Permission) -> Result<(), PlatformError> {
        match permission {
            Permission::ScreenRecording => {
                CGRequestScreenCaptureAccess();
            }
            Permission::Accessibility => {
                let key: &CFString = {
                    // SAFETY: an immutable CFString constant exported by ApplicationServices.
                    unsafe { kAXTrustedCheckOptionPrompt }
                };
                let options = CFDictionary::<CFString, CFBoolean>::from_slices(
                    &[key],
                    &[CFBoolean::new(true)],
                );
                // SAFETY: the dictionary maps CFString keys to CFBoolean values, as the API expects.
                unsafe { AXIsProcessTrustedWithOptions(Some(options.as_opaque())) };
                CGRequestPostEventAccess();
            }
            Permission::InputMonitoring => {
                CGRequestListenEventAccess();
            }
            _ => return Err(PlatformError::Unsupported("not a macOS permission")),
        }
        Ok(())
    }

    fn subscribe(
        &mut self,
        sink: Arc<dyn EventSink<(Permission, PermissionState)>>,
    ) -> Result<(), PlatformError> {
        if self.stop.is_some() {
            return Err(PlatformError::Backend(
                "Permissions::subscribe called twice".into(),
            ));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        std::thread::Builder::new()
            .name("tcc-poll".into())
            .spawn(move || {
                let mut last = HashMap::new();
                while !thread_stop.load(Ordering::Acquire) {
                    for permission in REQUIRED {
                        let now = state(permission);
                        if last.insert(permission, now) != Some(now) {
                            sink.send((permission, now));
                        }
                    }
                    std::thread::sleep(POLL);
                }
            })
            .map_err(|e| PlatformError::Backend(format!("spawn TCC poll thread: {e}")))?;
        self.stop = Some(stop);
        Ok(())
    }
}
