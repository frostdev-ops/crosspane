//! The production Mac probes (lead ruling 2026-10-04): public, read-only framework calls only.
//!
//! - [`SecuritySignatures`] verifies a file's code signature with the public Security framework
//!   (`SecStaticCodeCreateWithPath`, `SecRequirementCreateWithString`,
//!   `SecStaticCodeCheckValidity`). It checks strictly, across all architectures and nested
//!   code, against the designated requirement handed in by the caller, which comes from the
//!   build's trusted `ApprovedInventory` and never from the payload's own manifest. Any error
//!   leaves the observation unverified: nothing here ever reports a signature it couldn't check.
//! - [`SessionSupport`] observes the macOS version, the processor, the GUI session and its
//!   temporary folder. The console user and the interactive session come from two independent
//!   public sources: SystemConfiguration's `SCDynamicStoreCopyConsoleUser` (the primary console's
//!   user) and CoreGraphics' `CGSessionCopyCurrentDictionary` (this process's window-server
//!   session: its user, whether it is on the console and whether login finished). Anything
//!   unknown is reported as unavailable, which refuses.
//!
//! Neither probe prompts, writes, or touches TCC, the keychain or any other process.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};

use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, CFURL,
};
use objc2_core_graphics::CGSessionCopyCurrentDictionary;
use objc2_foundation::{NSProcessInfo, NSTemporaryDirectory};
use objc2_security::{
    SecCSFlags, SecCode, SecRequirement, SecStaticCode, kSecCSCheckAllArchitectures,
    kSecCSCheckNestedCode, kSecCSRequirementInformation, kSecCSSigningInformation,
    kSecCSStrictValidate, kSecCodeInfoEntitlementsDict, kSecCodeInfoFlags, kSecCodeInfoIdentifier,
    kSecCodeInfoTeamIdentifier,
};

use super::super::native_io::{
    Deadline, GuiObservation, NativeError, NativeResult, SignatureObservation, SignatureProbe,
    SigningRequirement, SupportObservation, SupportProbe,
};

/// Strict validation of every architecture and all nested code (frameworks, helpers).
const STRICT: u32 = kSecCSStrictValidate | kSecCSCheckAllArchitectures | kSecCSCheckNestedCode;
/// `kSecCodeSignatureAdhoc` and `kSecCodeSignatureRuntime` (CSCommon.h).
const FLAG_ADHOC: i64 = 0x0002;
const FLAG_RUNTIME: i64 = 0x0001_0000;
/// The Tier 1 signing identity (INSTALLER-rulings): an Apple-anchored Apple Development leaf.
const APPLE_DEVELOPMENT: &str =
    "anchor apple generic and certificate leaf[subject.CN] = \"Apple Development:\"*";
const MAX_ENTITLEMENTS: usize = 64;
const MAX_TEXT: usize = 4096;

// ---- signatures --------------------------------------------------------------------------------

/// The production [`SignatureProbe`] over the public Security framework.
#[derive(Debug, Default)]
pub struct SecuritySignatures;

impl SignatureProbe for SecuritySignatures {
    fn observe(
        &self,
        path: &Path,
        approved: &SigningRequirement,
        deadline: &Deadline,
    ) -> NativeResult<SignatureObservation> {
        deadline.check()?;
        let observation = signature_of(path, &approved.designated_requirement)?;
        deadline.check()?;
        Ok(observation)
    }
}

/// Observe `path`'s signature. `strict_verified` is true only when the strict check against the
/// approved requirement succeeds; the code's own designated requirement is reported as the
/// approved text only when both compile to the same canonical requirement.
pub(crate) fn signature_of(path: &Path, approved: &str) -> NativeResult<SignatureObservation> {
    if approved.is_empty() || approved.len() > MAX_TEXT {
        return Err(NativeError::Invalid);
    }
    let code = static_code(path).ok_or(NativeError::Unavailable)?;
    let info = signing_information(&code).ok_or(NativeError::Unavailable)?;
    let approved_requirement = requirement(approved);
    let strict_verified = approved_requirement
        .as_ref()
        .is_some_and(|r| satisfies(&code, r));
    let own = designated_requirement(&code);
    let designated_requirement = match (&own, approved_requirement.as_ref().and_then(canonical)) {
        (Some(own), Some(wanted)) if *own == wanted => approved.to_owned(),
        (Some(own), _) => own.clone(),
        (None, _) => String::new(),
    };
    let apple_development = requirement(APPLE_DEVELOPMENT).is_some_and(|r| satisfies(&code, &r));
    Ok(SignatureObservation {
        strict_verified,
        team_identifier: info.team,
        identifier: info.identifier,
        designated_requirement,
        entitlements: info.entitlements,
        apple_development,
        hardened_runtime: info.flags & FLAG_RUNTIME != 0,
        ad_hoc: info.flags & FLAG_ADHOC != 0,
    })
}

fn static_code(path: &Path) -> Option<CFRetained<SecStaticCode>> {
    let url = CFURL::from_file_path(path)?;
    let mut out: *const SecStaticCode = ptr::null();
    // SAFETY: `url` is a valid CFURL and `out` a valid out-pointer for the created object.
    let status =
        unsafe { SecStaticCode::create_with_path(&url, SecCSFlags(0), NonNull::from(&mut out)) };
    if status != 0 {
        return None;
    }
    // SAFETY: on success the function returns a +1 (Create rule) SecStaticCode reference.
    NonNull::new(out.cast_mut()).map(|code| unsafe { CFRetained::from_raw(code) })
}

fn requirement(text: &str) -> Option<CFRetained<SecRequirement>> {
    let text = CFString::from_str(text);
    let mut out: *mut SecRequirement = ptr::null_mut();
    // SAFETY: `text` is a valid CFString and `out` a valid out-pointer.
    let status = unsafe {
        SecRequirement::create_with_string(&text, SecCSFlags(0), NonNull::from(&mut out))
    };
    if status != 0 {
        return None;
    }
    // SAFETY: on success the function returns a +1 (Create rule) SecRequirement reference.
    NonNull::new(out).map(|r| unsafe { CFRetained::from_raw(r) })
}

/// The strict check: every architecture, all nested code, against `requirement`. Any non-zero
/// status (including an unsigned or modified file) is a failure.
fn satisfies(code: &SecStaticCode, requirement: &SecRequirement) -> bool {
    // SAFETY: both are valid, retained Security objects for the duration of the call.
    unsafe { code.check_validity(SecCSFlags(STRICT), Some(requirement)) == 0 }
}

fn canonical(requirement: &CFRetained<SecRequirement>) -> Option<String> {
    let mut out: *const CFString = ptr::null();
    // SAFETY: `requirement` is a valid SecRequirement and `out` a valid out-pointer.
    let status = unsafe { requirement.copy_string(SecCSFlags(0), NonNull::from(&mut out)) };
    if status != 0 {
        return None;
    }
    // SAFETY: on success the function returns a +1 (Copy rule) CFString.
    let text = NonNull::new(out.cast_mut()).map(|s| unsafe { CFRetained::from_raw(s) })?;
    let text = text.to_string();
    (text.len() <= MAX_TEXT).then_some(text)
}

fn designated_requirement(code: &SecStaticCode) -> Option<String> {
    let mut out: *mut SecRequirement = ptr::null_mut();
    // SAFETY: `code` is a valid SecStaticCode and `out` a valid out-pointer.
    let status = unsafe {
        SecCode::copy_designated_requirement(code, SecCSFlags(0), NonNull::from(&mut out))
    };
    if status != 0 {
        return None;
    }
    // SAFETY: on success the function returns a +1 (Copy rule) SecRequirement.
    let requirement = NonNull::new(out).map(|r| unsafe { CFRetained::from_raw(r) })?;
    canonical(&requirement)
}

struct SigningInfo {
    identifier: String,
    team: String,
    flags: i64,
    entitlements: BTreeMap<String, bool>,
}

fn signing_information(code: &SecStaticCode) -> Option<SigningInfo> {
    let mut out: *const CFDictionary = ptr::null();
    // SAFETY: `code` is a valid SecStaticCode and `out` a valid out-pointer.
    let status = unsafe {
        SecCode::copy_signing_information(
            code,
            SecCSFlags(kSecCSSigningInformation | kSecCSRequirementInformation),
            NonNull::from(&mut out),
        )
    };
    if status != 0 {
        return None;
    }
    // SAFETY: on success the function returns a +1 (Copy rule) CFDictionary.
    let info = NonNull::new(out.cast_mut()).map(|d| unsafe { CFRetained::from_raw(d) })?;
    // SAFETY: signing-information dictionaries have CFString keys; each value is type-checked.
    let info = unsafe { info.cast_unchecked::<CFString, CFType>() };
    // SAFETY: immutable public Security framework constants, retained by the framework.
    let (identifier_key, team_key, flags_key, entitlements_key) = unsafe {
        (
            kSecCodeInfoIdentifier,
            kSecCodeInfoTeamIdentifier,
            kSecCodeInfoFlags,
            kSecCodeInfoEntitlementsDict,
        )
    };
    let text = |key: &CFString| {
        info.get(key)
            .and_then(|v| v.downcast::<CFString>().ok())
            .map(|s| s.to_string())
            .filter(|s| s.len() <= MAX_TEXT)
    };
    // An unsigned file has no identifier: there is nothing to verify.
    let identifier = text(identifier_key)?;
    let team = text(team_key).unwrap_or_default();
    let flags = info
        .get(flags_key)
        .and_then(|v| v.downcast::<CFNumber>().ok())
        .and_then(|n| n.as_i64())?;
    let entitlements = match info.get(entitlements_key) {
        None => BTreeMap::new(),
        Some(value) => {
            let dictionary = value.downcast::<CFDictionary>().ok()?;
            // SAFETY: entitlement dictionaries have CFString keys; values are checked below.
            let dictionary = unsafe { dictionary.cast_unchecked::<CFString, CFType>() };
            if dictionary.len() > MAX_ENTITLEMENTS {
                return None;
            }
            let (keys, values) = dictionary.to_vecs();
            let mut out = BTreeMap::new();
            for (key, value) in keys.iter().zip(values) {
                // Only boolean entitlements are representable; anything else can't match the
                // approved list, so it fails closed.
                let value = value.downcast::<CFBoolean>().ok()?;
                out.insert(key.to_string(), value.as_bool());
            }
            out
        }
    };
    Some(SigningInfo {
        identifier,
        team,
        flags,
        entitlements,
    })
}

// ---- the GUI session ---------------------------------------------------------------------------

#[link(name = "SystemConfiguration", kind = "framework")]
unsafe extern "C" {
    /// SCDynamicStoreCopySpecific.h: the primary console's user name (+1), uid and gid.
    fn SCDynamicStoreCopyConsoleUser(
        store: *const c_void,
        uid: *mut u32,
        gid: *mut u32,
    ) -> *const CFString;
}

/// The primary console's user, from SystemConfiguration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConsoleUser {
    pub uid: u32,
    pub name: String,
}

/// This process's window-server session, from CoreGraphics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InteractiveSession {
    pub uid: u32,
    pub name: String,
    pub on_console: bool,
    pub login_done: bool,
}

fn console_user() -> Option<ConsoleUser> {
    let mut uid = u32::MAX;
    let mut gid = u32::MAX;
    // SAFETY: a null store selects the default session store; both out-pointers are valid.
    let name = unsafe { SCDynamicStoreCopyConsoleUser(ptr::null(), &mut uid, &mut gid) };
    // SAFETY: a non-null result is a +1 (Copy rule) CFString.
    let name = NonNull::new(name.cast_mut()).map(|s| unsafe { CFRetained::from_raw(s) })?;
    let name = name.to_string();
    (uid != u32::MAX && !name.is_empty() && name.len() <= 255).then_some(ConsoleUser { uid, name })
}

fn interactive_session() -> Option<InteractiveSession> {
    // Not in a window-server session (for example over SSH): there is no GUI session to admit.
    let session = CGSessionCopyCurrentDictionary()?;
    // SAFETY: session dictionaries have CFString keys; each value is type-checked below.
    let session = unsafe { session.cast_unchecked::<CFString, CFType>() };
    // CGSession.h defines these keys as CFSTR macros, so they are spelled here verbatim.
    let get = |key: &str| session.get(&CFString::from_str(key));
    let flag = |key: &str| {
        get(key)
            .and_then(|v| v.downcast::<CFBoolean>().ok())
            .map(|b| b.as_bool())
    };
    let uid = get("kCGSSessionUserIDKey")
        .and_then(|v| v.downcast::<CFNumber>().ok())
        .and_then(|n| n.as_i64())
        .and_then(|n| u32::try_from(n).ok())?;
    let name = get("kCGSSessionUserNameKey")
        .and_then(|v| v.downcast::<CFString>().ok())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty() && s.len() <= 255)?;
    Some(InteractiveSession {
        uid,
        name,
        on_console: flag("kCGSSessionOnConsoleKey")?,
        login_done: flag("kCGSessionLoginDoneKey")?,
    })
}

/// The session facts `admit_support` compares. Unknown on either side is unavailable, never a
/// guess. The session token is the user each source names: the two sources are independent, so
/// it matches only when this process's session belongs to the console's user.
pub(crate) fn gui_observation(
    console: Option<ConsoleUser>,
    interactive: Option<InteractiveSession>,
) -> NativeResult<GuiObservation> {
    let (Some(console), Some(interactive)) = (console, interactive) else {
        return Err(NativeError::Unavailable);
    };
    Ok(GuiObservation {
        console_uid: Some(console.uid),
        interactive_uid: Some(interactive.uid),
        console_session: format!("{}:{}", console.uid, console.name),
        interactive_session: format!("{}:{}", interactive.uid, interactive.name),
        active: interactive.on_console && interactive.login_done,
    })
}

/// This user's GUI temporary folder (`NSTemporaryDirectory`), without a trailing slash.
pub(crate) fn gui_tmpdir() -> Option<PathBuf> {
    let text = NSTemporaryDirectory().to_string();
    let trimmed = text.trim_end_matches('/');
    (!trimmed.is_empty() && trimmed.starts_with('/')).then(|| PathBuf::from(trimmed))
}

/// The production [`SupportProbe`].
#[derive(Debug, Default)]
pub struct SessionSupport;

impl SupportProbe for SessionSupport {
    fn observe(&self, deadline: &Deadline) -> NativeResult<SupportObservation> {
        deadline.check()?;
        let version = NSProcessInfo::processInfo().operatingSystemVersion();
        let macos_major =
            u16::try_from(version.majorVersion).map_err(|_| NativeError::Unavailable)?;
        let gui = gui_observation(console_user(), interactive_session())?;
        let gui_tmpdir = gui_tmpdir().ok_or(NativeError::Unavailable)?;
        deadline.check()?;
        Ok(SupportObservation {
            macos_major,
            // An arm64 build can't run on an Intel Mac; an Intel build (under Rosetta or not)
            // reports false, which refuses.
            apple_silicon: cfg!(target_arch = "aarch64"),
            gui,
            gui_tmpdir,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::sync::Arc;

    use super::super::super::native_io::{ArtifactRole, Cancellation, MonotonicClock};
    use super::*;

    fn deadline() -> Deadline {
        Deadline::new(
            10_000,
            Arc::new(MonotonicClock::default()),
            Cancellation::default(),
        )
        .unwrap()
    }

    fn rule(designated: &str) -> SigningRequirement {
        SigningRequirement {
            role: ArtifactRole::Installer,
            identifier: "com.apple.ls".into(),
            designated_requirement: designated.into(),
            entitlements: BTreeMap::new(),
        }
    }

    const LS: &str = "identifier \"com.apple.ls\" and anchor apple";

    #[test]
    fn an_apple_signed_binary_verifies_strictly_against_its_approved_requirement() {
        let seen = SecuritySignatures
            .observe(Path::new("/bin/ls"), &rule(LS), &deadline())
            .unwrap();
        assert!(seen.strict_verified, "{seen:?}");
        assert_eq!(seen.identifier, "com.apple.ls");
        assert_eq!(seen.designated_requirement, LS);
        assert!(!seen.ad_hoc);
        // A platform binary is not an Apple Development build with a Team: the producer's
        // admission still refuses it, as it must for anything not signed for Crosspane.
        assert!(!seen.apple_development);
        assert!(seen.admit(&rule(LS)).is_err());
    }

    #[test]
    fn a_mismatched_or_invalid_requirement_never_verifies() {
        for wrong in [
            "identifier \"com.example.nope\" and anchor apple",
            "identifier \"com.apple.ls\" and anchor apple generic and certificate leaf[subject.CN] = \"Apple Development:\"*",
            "this is not a requirement",
        ] {
            let seen = SecuritySignatures
                .observe(Path::new("/bin/ls"), &rule(wrong), &deadline())
                .unwrap();
            assert!(!seen.strict_verified, "{wrong}");
            assert_ne!(seen.designated_requirement, wrong, "{wrong}");
        }
    }

    #[test]
    fn a_missing_or_unsigned_file_is_unavailable_and_an_empty_requirement_is_invalid() {
        assert_eq!(
            SecuritySignatures
                .observe(Path::new("/nonexistent/crosspane"), &rule(LS), &deadline())
                .unwrap_err(),
            NativeError::Unavailable
        );
        let dir = std::env::temp_dir().join(format!("cp421-unsigned-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("plain");
        std::fs::write(&file, b"#!/bin/sh\nexit 0\n").unwrap();
        let unsigned = SecuritySignatures.observe(&file, &rule(LS), &deadline());
        assert!(
            unsigned.is_err() || unsigned.as_ref().is_ok_and(|s| !s.strict_verified),
            "{unsigned:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            signature_of(Path::new("/bin/ls"), "").unwrap_err(),
            NativeError::Invalid
        );
    }

    fn console(uid: u32, name: &str) -> ConsoleUser {
        ConsoleUser {
            uid,
            name: name.into(),
        }
    }

    fn session(uid: u32, name: &str, on_console: bool, login_done: bool) -> InteractiveSession {
        InteractiveSession {
            uid,
            name: name.into(),
            on_console,
            login_done,
        }
    }

    #[test]
    fn an_unknown_console_or_session_is_unavailable() {
        assert_eq!(
            gui_observation(None, Some(session(501, "a", true, true))).unwrap_err(),
            NativeError::Unavailable
        );
        assert_eq!(
            gui_observation(Some(console(501, "a")), None).unwrap_err(),
            NativeError::Unavailable
        );
    }

    #[test]
    fn the_two_sources_match_only_for_the_console_users_own_finished_session() {
        let ok =
            gui_observation(Some(console(501, "a")), Some(session(501, "a", true, true))).unwrap();
        assert!(ok.active);
        assert_eq!(ok.console_uid, Some(501));
        assert_eq!(ok.interactive_uid, Some(501));
        assert_eq!(ok.console_session, ok.interactive_session);
        // Another user on the console (fast user switching): the sessions differ.
        let other =
            gui_observation(Some(console(502, "b")), Some(session(501, "a", true, true))).unwrap();
        assert_ne!(other.console_session, other.interactive_session);
        assert_ne!(other.console_uid, other.interactive_uid);
        // Not on the console, or login not finished: never active.
        for (on, done) in [(false, true), (true, false), (false, false)] {
            let seen = gui_observation(Some(console(501, "a")), Some(session(501, "a", on, done)))
                .unwrap();
            assert!(!seen.active, "{on} {done}");
        }
    }

    #[test]
    fn the_live_session_probe_is_read_only_and_either_observes_or_is_unavailable() {
        match SessionSupport.observe(&deadline()) {
            Ok(seen) => {
                assert!(seen.macos_major >= 11, "{}", seen.macos_major);
                assert!(seen.gui_tmpdir.is_absolute());
                assert!(!seen.gui_tmpdir.to_string_lossy().ends_with('/'));
            }
            // Over SSH there is no window-server session to observe.
            Err(error) => assert_eq!(error, NativeError::Unavailable),
        }
    }
}
