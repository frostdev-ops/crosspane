//! TCC status and onboarding (04 §7, WP-1.20) through the OS's own preflight and request APIs. Never
//! reads or edits the TCC database.
//!
//! Grants belong to the code-signing identity of the *responsible* process, so the agent must run as
//! the signed `Crosspane.app` bundle (`io.frostdev.crosspane.agent`, scripts/macos/bundle.sh), not as
//! a bare binary under a terminal or SSH.
//!
//! Microphone (WP-4.13a) is the fourth grant: reading the hidden "Crosspane speakers" loopback
//! input counts as microphone input to TCC (scripts/macos/bundle.sh). The physical microphone is
//! never opened, and nothing here shares it; this module only reports and requests the grant.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use block2::RcBlock;
use crosspane_platform::{EventSink, Permission, PermissionState, Permissions, PlatformError};
use objc2::runtime::Bool;
use objc2_application_services::{
    AXIsProcessTrusted, AXIsProcessTrustedWithOptions, kAXTrustedCheckOptionPrompt,
};
use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFString};
use objc2_core_graphics::{
    CGPreflightListenEventAccess, CGPreflightPostEventAccess, CGPreflightScreenCaptureAccess,
    CGRequestListenEventAccess, CGRequestPostEventAccess, CGRequestScreenCaptureAccess,
};

/// How often `subscribe` re-reads the status. TCC has no change notification.
const POLL: Duration = Duration::from_secs(1);

/// The grants every Mac agent needs. Microphone follows them when audio is on.
const BASE_REQUIRED: [Permission; 3] = [
    Permission::ScreenRecording,
    Permission::Accessibility,
    Permission::InputMonitoring,
];

/// The required grants, in the order they are reported: the three base grants, then Microphone
/// when `microphone` is set.
fn required_list(microphone: bool) -> Vec<Permission> {
    let mut required = BASE_REQUIRED.to_vec();
    if microphone {
        required.push(Permission::Microphone);
    }
    required
}

#[derive(Debug)]
pub struct MacPermissions {
    /// Whether Microphone is part of the required list (the agent turns it off with
    /// `CROSSPANE_AUDIO=0`).
    microphone: bool,
    stop: Option<Arc<AtomicBool>>,
}

impl MacPermissions {
    /// `microphone` adds Microphone to the required list: the loopback input that carries this
    /// Mac's speaker audio is gated by it.
    pub fn new(microphone: bool) -> MacPermissions {
        MacPermissions {
            microphone,
            stop: None,
        }
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
            Permission::Microphone => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
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

/// Microphone status from the raw `AVAuthorizationStatus`. "Not determined" is not granted: the
/// user has not been asked yet, and nothing is captured until they say yes. Anything the SDK did
/// not define is `Unknown`, never `Granted`.
fn microphone_state_from_status(status: AVAuthorizationStatus) -> PermissionState {
    match status {
        AVAuthorizationStatus::Authorized => PermissionState::Granted,
        AVAuthorizationStatus::Denied
        | AVAuthorizationStatus::Restricted
        | AVAuthorizationStatus::NotDetermined => PermissionState::NotGranted,
        _ => PermissionState::Unknown,
    }
}

/// Read-only: asks AVFoundation for the audio-capture authorization status. Opens no device and
/// shows no dialog.
fn microphone_state() -> PermissionState {
    // SAFETY: an immutable NSString constant exported by AVFoundation (nullable in the bindings).
    let Some(audio) = (unsafe { AVMediaTypeAudio }) else {
        return PermissionState::Unknown;
    };
    // SAFETY: `audio` is AVMediaTypeAudio, one of the two media types the method accepts; the
    // class method only reads this process's authorization status and may be called from any
    // thread.
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(audio) };
    microphone_state_from_status(status)
}

/// Ask the OS for audio-capture access. Returns once the request has started; the system dialog
/// (when one is due) is answered later, and the grant shows up in `state`. Already-answered
/// requests show nothing, and the caller points the user at System Settings instead.
fn request_microphone_access() -> Result<(), PlatformError> {
    // SAFETY: an immutable NSString constant exported by AVFoundation (nullable in the bindings).
    let Some(audio) = (unsafe { AVMediaTypeAudio }) else {
        return Err(PlatformError::Unsupported(
            "AVFoundation has no audio media type",
        ));
    };
    // The handler captures nothing, so it is `'static` and safe to run on whatever dispatch queue
    // AVFoundation picks. It only logs.
    let handler = RcBlock::new(|granted: Bool| {
        tracing::debug!(
            granted = granted.as_bool(),
            "microphone access request answered"
        );
    });
    // SAFETY: `audio` is AVMediaTypeAudio, a media type the method accepts; the block has the
    // documented `void (^)(BOOL)` signature, and AVFoundation copies it to keep it past the call.
    unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(audio, &handler) };
    Ok(())
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
        Permission::Microphone => return microphone_state(),
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
        required_list(self.microphone)
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
            Permission::Microphone => request_microphone_access()?,
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
        let required = self.required();
        std::thread::Builder::new()
            .name("tcc-poll".into())
            .spawn(move || {
                let mut last = HashMap::new();
                while !thread_stop.load(Ordering::Acquire) {
                    for &permission in &required {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microphone_status_mapping() {
        use PermissionState::{Granted, NotGranted, Unknown};
        // The raw values are the SDK's AVAuthorizationStatus: 0 NotDetermined, 1 Restricted,
        // 2 Denied, 3 Authorized.
        assert_eq!(
            microphone_state_from_status(AVAuthorizationStatus(3)),
            Granted
        );
        assert_eq!(
            microphone_state_from_status(AVAuthorizationStatus(2)),
            NotGranted
        );
        assert_eq!(
            microphone_state_from_status(AVAuthorizationStatus(1)),
            NotGranted
        );
        assert_eq!(
            microphone_state_from_status(AVAuthorizationStatus(0)),
            NotGranted
        );
        // A value the SDK did not define is never read as a grant.
        assert_eq!(
            microphone_state_from_status(AVAuthorizationStatus(4)),
            Unknown
        );
        assert_eq!(
            microphone_state_from_status(AVAuthorizationStatus(-1)),
            Unknown
        );
        assert_eq!(
            microphone_state_from_status(AVAuthorizationStatus::Authorized),
            Granted
        );
    }

    #[test]
    fn required_list_has_microphone_last_when_asked() {
        let without = MacPermissions::new(false).required();
        assert_eq!(
            without,
            [
                Permission::ScreenRecording,
                Permission::Accessibility,
                Permission::InputMonitoring
            ]
        );
        let with = MacPermissions::new(true).required();
        assert_eq!(with.len(), 4);
        assert_eq!(&with[..3], &without[..]);
        assert_eq!(with.last(), Some(&Permission::Microphone));
    }

    #[test]
    fn microphone_settings_pane() {
        assert_eq!(
            MacPermissions::settings_url(Permission::Microphone),
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
        );
    }

    /// Read-only: the status read opens no device and shows no dialog. On a real macOS the
    /// authorization status is always one of the SDK's four values, so it is never `Unknown`.
    /// (`request` is deliberately not called: it can show a system dialog.)
    #[test]
    fn microphone_state_reads_a_known_status() {
        let perms = MacPermissions::new(true);
        assert_ne!(
            perms.state(Permission::Microphone),
            PermissionState::Unknown
        );
    }
}
