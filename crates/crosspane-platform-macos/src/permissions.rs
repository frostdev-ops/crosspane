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
//!
//! Onboarding (WP-4.33) asks for one grant at a time: `request` starts exactly one OS flow, and
//! [`MacPermissions::prior`] says whether the OS can still show that grant's one-time prompt, so
//! the agent knows when to open System Settings instead. [`raw_states`] reads every underlying
//! API separately for the agent's change log. Nothing here decides when to ask.

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
    CGRequestScreenCaptureAccess,
};

/// How often `subscribe` re-reads the status. TCC has no change notification.
const POLL: Duration = Duration::from_secs(1);

/// The bundle identifier of the signed agent (scripts/macos/bundle.sh). TCC keys every grant to it.
pub const AGENT_BUNDLE_ID: &str = "io.frostdev.crosspane.agent";

/// `IOHIDRequestType` (IOKit/hidsystem/IOHIDLib.h).
const IOHID_REQUEST_POST_EVENT: u32 = 0;
const IOHID_REQUEST_LISTEN_EVENT: u32 = 1;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    /// Returns an `IOHIDAccessType`: 0 granted, 1 denied, 2 unknown (never asked). macOS 10.15+.
    fn IOHIDCheckAccess(request_type: u32) -> u32;
    /// Shows the one-time Input Monitoring prompt when the user has not answered it yet. Returns
    /// whether access is granted. macOS 10.15+.
    fn IOHIDRequestAccess(request_type: u32) -> bool;
}

/// One of IOKit's three access answers (`IOHIDCheckAccess`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HidAccess {
    Granted,
    /// The user answered no, or turned it off in System Settings. The prompt won't show again.
    Denied,
    /// Never asked: the one-time prompt can still appear.
    Unknown,
}

impl HidAccess {
    fn from_raw(raw: u32) -> HidAccess {
        match raw {
            0 => HidAccess::Granted,
            1 => HidAccess::Denied,
            // 2 is "unknown"; anything the SDK did not define is never read as a grant.
            _ => HidAccess::Unknown,
        }
    }

    /// The token the agent logs.
    pub fn token(self) -> &'static str {
        match self {
            HidAccess::Granted => "granted",
            HidAccess::Denied => "denied",
            HidAccess::Unknown => "unknown",
        }
    }
}

/// Input Monitoring's three states (`IOHIDCheckAccess(kIOHIDRequestTypeListenEvent)`).
pub fn input_monitoring_access() -> HidAccess {
    // SAFETY: takes a plain enum value and only reads this process's TCC status; shows nothing.
    HidAccess::from_raw(unsafe { IOHIDCheckAccess(IOHID_REQUEST_LISTEN_EVENT) })
}

/// Whether the OS can still show a grant's one-time prompt to this identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prior {
    /// The user has not been asked: the OS's request shows its prompt.
    NeverAsked,
    /// The user already answered (or changed it in System Settings): a request shows nothing,
    /// only the System Settings pane can change it.
    Answered,
    /// The OS doesn't say (Accessibility, Screen Recording).
    Unknowable,
}

/// The raw answer of every API the grants are read from, for the agent's change log. Holds no
/// content, only yes/no and status tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawStates {
    /// `AXIsProcessTrusted()`: the Accessibility grant.
    pub ax_trusted: bool,
    /// `CGPreflightPostEventAccess()`: what the injector checks before posting events.
    pub cg_post_event: bool,
    /// `IOHIDCheckAccess(kIOHIDRequestTypePostEvent)`.
    pub hid_post_event: HidAccess,
    /// `IOHIDCheckAccess(kIOHIDRequestTypeListenEvent)`: the Input Monitoring grant.
    pub hid_listen_event: HidAccess,
    /// `CGPreflightListenEventAccess()`.
    pub cg_listen_event: bool,
    /// `CGPreflightScreenCaptureAccess()`: the Screen Recording grant (may be stale in-process).
    pub cg_screen_capture: bool,
    /// The raw `AVAuthorizationStatus` for audio (0 not determined, 1 restricted, 2 denied,
    /// 3 authorized), or -1 when AVFoundation has no audio media type.
    pub microphone: isize,
}

impl std::fmt::Display for RawStates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ax_trusted={} cg_post_event={} hid_post_event={} hid_listen_event={} \
             cg_listen_event={} cg_screen_capture={} av_audio_status={}",
            self.ax_trusted,
            self.cg_post_event,
            self.hid_post_event.token(),
            self.hid_listen_event.token(),
            self.cg_listen_event,
            self.cg_screen_capture,
            self.microphone,
        )
    }
}

/// Read every underlying API once. Read-only: shows no dialog and opens no device.
pub fn raw_states() -> RawStates {
    RawStates {
        // SAFETY: takes no arguments and only reads this process's trust status.
        ax_trusted: unsafe { AXIsProcessTrusted() },
        cg_post_event: CGPreflightPostEventAccess(),
        // SAFETY: as in `input_monitoring_access`.
        hid_post_event: HidAccess::from_raw(unsafe { IOHIDCheckAccess(IOHID_REQUEST_POST_EVENT) }),
        hid_listen_event: input_monitoring_access(),
        cg_listen_event: CGPreflightListenEventAccess(),
        cg_screen_capture: CGPreflightScreenCaptureAccess(),
        microphone: microphone_status().map_or(-1, |s| s.0),
    }
}

/// The `tccutil reset` service name for `permission` (`tccutil reset <service> <bundle id>`).
pub fn tcc_service(permission: Permission) -> Option<&'static str> {
    Some(match permission {
        Permission::ScreenRecording => "ScreenCapture",
        Permission::Accessibility => "Accessibility",
        Permission::InputMonitoring => "ListenEvent",
        Permission::Microphone => "Microphone",
        _ => return None,
    })
}

/// This process's own bundle identifier, when it runs from an app bundle.
pub fn own_bundle_id() -> Option<String> {
    let bundle = objc2_foundation::NSBundle::mainBundle();
    bundle.bundleIdentifier().map(|id| id.to_string())
}

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

    /// Whether the OS can still show `permission`'s one-time prompt (Input Monitoring and
    /// Microphone say so; Accessibility and Screen Recording don't). Read-only.
    pub fn prior(permission: Permission) -> Prior {
        match permission {
            Permission::InputMonitoring => match input_monitoring_access() {
                HidAccess::Unknown => Prior::NeverAsked,
                HidAccess::Granted | HidAccess::Denied => Prior::Answered,
            },
            Permission::Microphone => match microphone_status() {
                Some(AVAuthorizationStatus::NotDetermined) => Prior::NeverAsked,
                Some(_) => Prior::Answered,
                None => Prior::Unknowable,
            },
            _ => Prior::Unknowable,
        }
    }

    /// The System Settings pane for `permission`, for when the one-time system prompt has already
    /// been answered and won't appear again. macOS 27 names the Accessibility pane "Device Control
    /// and Data Access"; the link is the same.
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
/// shows no dialog. `None` when AVFoundation has no audio media type.
fn microphone_status() -> Option<AVAuthorizationStatus> {
    // SAFETY: an immutable NSString constant exported by AVFoundation (nullable in the bindings).
    let audio = unsafe { AVMediaTypeAudio }?;
    // SAFETY: `audio` is AVMediaTypeAudio, one of the two media types the method accepts; the
    // class method only reads this process's authorization status and may be called from any
    // thread.
    Some(unsafe { AVCaptureDevice::authorizationStatusForMediaType(audio) })
}

fn microphone_state() -> PermissionState {
    microphone_status().map_or(PermissionState::Unknown, microphone_state_from_status)
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
/// the user changes it; the agent restarts itself once grants change (WP-4.33).
///
/// Accessibility is `AXIsProcessTrusted()` alone, which follows a System Settings toggle in a
/// running process (Rectangle and AltTab poll it). It used to be ANDed with
/// `CGPreflightPostEventAccess()`, which kept answering no after the 11:08 grant: tccd logged no
/// post-event lookup after the toggle, so it answers from within the process. The grant never
/// showed, so the agent never restarted (WP-4.33 root cause). The injector still checks
/// `CGPreflightPostEventAccess()` itself, in the fresh process after the restart.
///
/// Input Monitoring is granted when either IOKit's tri-state check or CoreGraphics' preflight says
/// so: both read the same grant, and IOKit's check is the one Karabiner polls for a live answer.
pub fn state(permission: Permission) -> PermissionState {
    let granted = match permission {
        Permission::ScreenRecording => CGPreflightScreenCaptureAccess(),
        // SAFETY: takes no arguments and only reads this process's trust status.
        Permission::Accessibility => unsafe { AXIsProcessTrusted() },
        Permission::InputMonitoring => {
            input_monitoring_access() == HidAccess::Granted || CGPreflightListenEventAccess()
        }
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

    /// Starts exactly one OS flow: the grant's own prompt when the OS still shows one. It never
    /// opens System Settings and never asks for a second grant on the side.
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
            }
            Permission::InputMonitoring => {
                // IOHIDRequestAccess may wait for the answer, so it runs off the caller's thread.
                std::thread::Builder::new()
                    .name("tcc-ask".into())
                    .spawn(|| {
                        // SAFETY: takes a plain enum value; shows the one-time prompt at most.
                        let granted = unsafe { IOHIDRequestAccess(IOHID_REQUEST_LISTEN_EVENT) };
                        tracing::debug!(granted, "input monitoring request answered");
                    })
                    .map_err(|e| PlatformError::Backend(format!("spawn TCC ask thread: {e}")))?;
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

    #[test]
    fn hid_access_mapping_never_reads_an_unknown_value_as_granted() {
        assert_eq!(HidAccess::from_raw(0), HidAccess::Granted);
        assert_eq!(HidAccess::from_raw(1), HidAccess::Denied);
        assert_eq!(HidAccess::from_raw(2), HidAccess::Unknown);
        assert_eq!(HidAccess::from_raw(7), HidAccess::Unknown);
    }

    #[test]
    fn tcc_services_and_panes() {
        assert_eq!(
            tcc_service(Permission::Accessibility),
            Some("Accessibility")
        );
        assert_eq!(
            tcc_service(Permission::ScreenRecording),
            Some("ScreenCapture")
        );
        assert_eq!(
            tcc_service(Permission::InputMonitoring),
            Some("ListenEvent")
        );
        assert_eq!(tcc_service(Permission::Microphone), Some("Microphone"));
        assert!(
            MacPermissions::settings_url(Permission::InputMonitoring)
                .ends_with("Privacy_ListenEvent")
        );
        assert!(
            MacPermissions::settings_url(Permission::Accessibility)
                .ends_with("Privacy_Accessibility")
        );
    }

    /// Read-only: every raw reading is a preflight or status query; none prompts. A test binary
    /// is not the signed agent, so nothing is granted to it and nothing is asked.
    #[test]
    fn raw_states_read_without_prompting() {
        let raw = raw_states();
        let line = raw.to_string();
        assert!(line.contains("ax_trusted="));
        assert!(line.contains("hid_listen_event="));
        // Prior is read-only too.
        let _ = MacPermissions::prior(Permission::InputMonitoring);
        let _ = MacPermissions::prior(Permission::Microphone);
        assert_eq!(
            MacPermissions::prior(Permission::Accessibility),
            Prior::Unknowable
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
