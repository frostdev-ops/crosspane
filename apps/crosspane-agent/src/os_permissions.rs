//! OS permission onboarding (WP-4.33): one permission per click, System Settings when no prompt
//! can appear, grant watching, and the one self-restart that makes a new grant take effect.
//!
//! The grants belong to the agent's own signed identity, so the agent asks, never the installer.
//! Nothing here asks on its own: every ask follows a click (installer, tray or `crosspanectl`).
//! Logs carry permission names and yes/no states only, never any content.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crosspane_platform::{Permission, PermissionState, Permissions};
use serde::Serialize;

/// The order permissions are asked in. Input Monitoring is asked only once Accessibility is on
/// (lead ruling): an Accessibility grant can make it unnecessary, and its prompt shows once.
pub const ASK_ORDER: [Permission; 4] = [
    Permission::Accessibility,
    Permission::InputMonitoring,
    Permission::ScreenRecording,
    Permission::Microphone,
];

/// How often grants are read. TCC has no change notification.
pub const POLL: Duration = Duration::from_secs(1);
/// After a change that leaves a grant still missing, the set must hold this long before the agent
/// restarts to use it (the person may be about to allow the next one).
pub const SETTLE: Duration = Duration::from_secs(10);
/// The Accessibility prompt is shown first; its pane follows this long after, unless granted.
pub const PANE_DELAY: Duration = Duration::from_secs(1);

/// The file that remembers which one-time prompts this machine has already shown. Only Screen
/// Recording needs it (the OS doesn't say); a reset clears its entry.
pub const ASK_RECORD: &str = "permission-asks";
/// The once-a-day burst's marker (removed in WP-4.33); deleted at start if an older agent left it.
pub const OLD_BURST_MARKER: &str = "permissions-requested";

/// The token `status`, ctl and the logs use for `permission`.
pub fn permission_token(permission: Permission) -> &'static str {
    match permission {
        Permission::ScreenRecording => "screen_recording",
        Permission::Accessibility => "accessibility",
        Permission::InputMonitoring => "input_monitoring",
        Permission::Microphone => "microphone",
        _ => "other",
    }
}

/// The token the logs use for a permission state.
pub fn state_token(state: PermissionState) -> &'static str {
    match state {
        PermissionState::Granted => "granted",
        PermissionState::NotGranted => "not_granted",
        PermissionState::Unknown => "unknown",
    }
}

/// The permission a ctl token names.
pub fn permission_named(name: &str) -> Result<Permission, String> {
    ASK_ORDER
        .into_iter()
        .find(|p| permission_token(*p) == name)
        .ok_or_else(|| {
            let known: Vec<&str> = ASK_ORDER.into_iter().map(permission_token).collect();
            format!("unknown permission {name}: use {}", known.join(", "))
        })
}

/// Whether the OS can still show a grant's one-time prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
// Only macOS says; elsewhere every prior is `Unknowable`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub enum Prior {
    NeverAsked,
    Answered,
    /// The OS doesn't say (Accessibility, Screen Recording).
    Unknowable,
}

/// What one ask put on the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Shown {
    /// Nothing: the permission is already granted.
    Nothing,
    /// The OS's own prompt.
    Prompt,
    /// System Settings at the permission's pane: its prompt was already answered.
    Pane,
    /// The prompt, then (about a second later, unless granted by then) the pane.
    PromptThenPane,
}

/// The answer to one ask, as ctl returns it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Asked {
    /// The permission asked for. It differs from the one requested when Accessibility had to
    /// come first.
    pub permission: Option<&'static str>,
    pub shown: Shown,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<&'static str>,
}

/// The raw API answers behind the grants, for the change log (no content), and the parts of
/// them that a running backend depends on beyond the grant set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Raw {
    pub text: String,
    pub backend_key: Vec<bool>,
}

/// The OS side of onboarding that the frozen `Permissions` trait doesn't cover. Real on macOS,
/// inert elsewhere, fakes in tests.
pub trait TccOps: Send {
    fn prior(&self, permission: Permission) -> Prior;
    fn open_pane(&mut self, permission: Permission);
    /// Open the pane after `after`, unless `permission` is granted by then. Never blocks.
    fn open_pane_unless_granted(&mut self, permission: Permission, after: Duration);
    /// `tccutil reset <service> <the agent's own bundle id>`, nothing else.
    fn reset(&mut self, permission: Permission) -> Result<(), String>;
    fn raw(&self) -> Option<Raw>;
}

/// Which one-time prompts were already shown, kept across restarts in the state directory.
#[derive(Debug, Default)]
pub struct AskRecord {
    path: Option<PathBuf>,
    asked: BTreeSet<String>,
    loaded: bool,
}

impl AskRecord {
    pub fn at(path: Option<PathBuf>) -> AskRecord {
        AskRecord {
            path,
            asked: BTreeSet::new(),
            loaded: false,
        }
    }

    fn load(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        if let Some(text) = self
            .path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
        {
            self.asked = text
                .lines()
                .map(str::trim)
                .filter(|l| permission_named(l).is_ok())
                .map(str::to_owned)
                .collect();
        }
    }

    fn save(&self) {
        if let Some(path) = &self.path {
            let text: String = self.asked.iter().map(|l| format!("{l}\n")).collect();
            if let Err(e) = std::fs::write(path, text) {
                tracing::debug!(error = %e, "could not record the permission ask");
            }
        }
    }

    pub fn contains(&mut self, permission: Permission) -> bool {
        self.load();
        self.asked.contains(permission_token(permission))
    }

    pub fn insert(&mut self, permission: Permission) {
        self.load();
        if self.asked.insert(permission_token(permission).to_owned()) {
            self.save();
        }
    }

    pub fn remove(&mut self, permission: Permission) {
        self.load();
        if self.asked.remove(permission_token(permission)) {
            self.save();
        }
    }
}

fn granted(perms: &dyn Permissions, permission: Permission) -> bool {
    perms.state(permission) == PermissionState::Granted
}

fn request(perms: &mut dyn Permissions, permission: Permission) -> Result<(), String> {
    perms
        .request(permission)
        .map_err(|e| format!("could not ask for {}: {e}", permission_token(permission)))
}

/// Ask for exactly one permission (lead rulings, WP-4.33):
/// - Accessibility: its prompt, then its pane about a second later unless granted by then.
/// - Input Monitoring: only once Accessibility is on (Accessibility is asked instead). Never
///   asked: its prompt. Denied: ask anyway, then its pane.
/// - Screen Recording and Microphone: the prompt if it was never shown, else the pane.
pub fn ask(
    perms: &mut dyn Permissions,
    ops: &mut dyn TccOps,
    record: &mut AskRecord,
    permission: Permission,
) -> Result<Asked, String> {
    let required = perms.required();
    if !required.contains(&permission) {
        return Err(format!(
            "{} is not needed on this machine",
            permission_token(permission)
        ));
    }
    if granted(perms, permission) {
        return Ok(Asked {
            permission: Some(permission_token(permission)),
            shown: Shown::Nothing,
            note: Some("already granted"),
        });
    }
    if permission == Permission::InputMonitoring
        && required.contains(&Permission::Accessibility)
        && !granted(perms, Permission::Accessibility)
    {
        let mut asked = ask(perms, ops, record, Permission::Accessibility)?;
        asked.note =
            Some("Accessibility comes first; Input Monitoring is asked once Accessibility is on");
        return Ok(asked);
    }
    let shown = match permission {
        Permission::Accessibility => {
            request(perms, permission)?;
            ops.open_pane_unless_granted(permission, PANE_DELAY);
            Shown::PromptThenPane
        }
        Permission::InputMonitoring => match ops.prior(permission) {
            Prior::NeverAsked => {
                request(perms, permission)?;
                Shown::Prompt
            }
            Prior::Answered | Prior::Unknowable => {
                // Asking again re-adds Crosspane to the list if its row was removed.
                request(perms, permission)?;
                ops.open_pane(permission);
                Shown::Pane
            }
        },
        Permission::Microphone => match ops.prior(permission) {
            Prior::NeverAsked => {
                request(perms, permission)?;
                Shown::Prompt
            }
            Prior::Answered | Prior::Unknowable => {
                ops.open_pane(permission);
                Shown::Pane
            }
        },
        Permission::ScreenRecording => {
            if record.contains(permission) {
                // Shows nothing once answered, but puts Crosspane back in the list after a reset.
                request(perms, permission)?;
                ops.open_pane(permission);
                Shown::Pane
            } else {
                request(perms, permission)?;
                Shown::Prompt
            }
        }
        _ => return Err("not a permission this machine asks for".into()),
    };
    record.insert(permission);
    tracing::info!(
        permission = permission_token(permission),
        ?shown,
        "asked the OS for a permission"
    );
    Ok(Asked {
        permission: Some(permission_token(permission)),
        shown,
        note: None,
    })
}

/// The first missing required permission, in [`ASK_ORDER`].
pub fn first_missing(perms: &dyn Permissions) -> Option<Permission> {
    let required = perms.required();
    ASK_ORDER
        .into_iter()
        .find(|p| required.contains(p) && !granted(perms, *p))
}

/// `AskPermissions`: ask for the first missing permission only.
pub fn ask_first_missing(
    perms: &mut dyn Permissions,
    ops: &mut dyn TccOps,
    record: &mut AskRecord,
) -> Result<Asked, String> {
    match first_missing(perms) {
        Some(permission) => ask(perms, ops, record, permission),
        None => Ok(Asked {
            permission: None,
            shown: Shown::Nothing,
            note: Some("every permission is granted"),
        }),
    }
}

/// `ResetPermission`, only ever on an explicit click: clear Crosspane's own entry for
/// `permission`, so its prompt can show again.
pub fn reset(
    perms: &dyn Permissions,
    ops: &mut dyn TccOps,
    record: &mut AskRecord,
    permission: Permission,
) -> Result<(), String> {
    if !perms.required().contains(&permission) {
        return Err(format!(
            "{} is not needed on this machine",
            permission_token(permission)
        ));
    }
    ops.reset(permission)?;
    record.remove(permission);
    tracing::info!(
        permission = permission_token(permission),
        "reset Crosspane's own permission entry on request"
    );
    Ok(())
}

/// What the grant watch compares: the granted set, whether it is complete, and the raw answers a
/// running backend depends on beyond the set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reading {
    pub granted: Vec<Permission>,
    pub complete: bool,
    pub backend_key: Vec<bool>,
}

impl Reading {
    pub fn of(perms: &dyn Permissions, raw: Option<&Raw>) -> Reading {
        let required = perms.required();
        let granted: Vec<Permission> = required
            .iter()
            .copied()
            .filter(|p| granted(perms, *p))
            .collect();
        Reading {
            complete: granted.len() == required.len(),
            granted,
            backend_key: raw.map(|r| r.backend_key.clone()).unwrap_or_default(),
        }
    }
}

/// Why the watch asks for a restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartReason {
    /// Every required grant is present now (seen on two polls in a row).
    Complete,
    /// The grants changed and held for [`SETTLE`].
    Settled,
}

/// Decides the one restart that makes a grant change take effect. The backends are built once,
/// at start, from the grants of that moment (`baseline`).
#[derive(Debug)]
pub struct GrantWatch {
    baseline: Reading,
    /// The latest reading that differs from the baseline, and since when it has held.
    pending: Option<(Reading, Instant)>,
}

impl GrantWatch {
    pub fn new(baseline: Reading) -> GrantWatch {
        GrantWatch {
            baseline,
            pending: None,
        }
    }

    pub fn baseline(&self) -> &Reading {
        &self.baseline
    }

    /// One poll. A complete set restarts once confirmed by a second poll; any other change
    /// restarts once it has held for [`SETTLE`]; a change that goes back cancels.
    pub fn observe(&mut self, now: Instant, reading: Reading) -> Option<RestartReason> {
        if reading == self.baseline {
            self.pending = None;
            return None;
        }
        match &self.pending {
            Some((seen, since)) if *seen == reading => {
                if reading.complete {
                    Some(RestartReason::Complete)
                } else if now.saturating_duration_since(*since) >= SETTLE {
                    Some(RestartReason::Settled)
                } else {
                    None
                }
            }
            _ => {
                self.pending = Some((reading, now));
                None
            }
        }
    }
}

/// The real OS side.
#[derive(Debug, Default)]
pub struct SystemTcc;

#[cfg(target_os = "macos")]
impl TccOps for SystemTcc {
    fn prior(&self, permission: Permission) -> Prior {
        use crosspane_platform_macos::permissions::{MacPermissions, Prior as Os};
        match MacPermissions::prior(permission) {
            Os::NeverAsked => Prior::NeverAsked,
            Os::Answered => Prior::Answered,
            Os::Unknowable => Prior::Unknowable,
        }
    }

    fn open_pane(&mut self, permission: Permission) {
        crate::open_settings_pane(permission);
    }

    fn open_pane_unless_granted(&mut self, permission: Permission, after: Duration) {
        let spawned = std::thread::Builder::new()
            .name("tcc-pane".into())
            .spawn(move || {
                std::thread::sleep(after);
                if crosspane_platform_macos::permissions::state(permission)
                    != PermissionState::Granted
                {
                    crate::open_settings_pane(permission);
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not schedule the System Settings pane");
        }
    }

    fn reset(&mut self, permission: Permission) -> Result<(), String> {
        use crosspane_platform_macos::permissions::{AGENT_BUNDLE_ID, own_bundle_id, tcc_service};
        let service = tcc_service(permission).ok_or("not a permission tccutil resets")?;
        // Only ever this agent's own entry: refused unless running as the signed Crosspane.app.
        match own_bundle_id() {
            Some(id) if id == AGENT_BUNDLE_ID => {}
            _ => {
                return Err(format!(
                    "reset refused: this agent is not running as {AGENT_BUNDLE_ID}"
                ));
            }
        }
        let status = std::process::Command::new("/usr/bin/tccutil")
            .args(["reset", service, AGENT_BUNDLE_ID])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| format!("could not run tccutil: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("tccutil reset {service} failed ({status})"))
        }
    }

    fn raw(&self) -> Option<Raw> {
        let raw = crosspane_platform_macos::permissions::raw_states();
        Some(Raw {
            text: raw.to_string(),
            backend_key: vec![raw.cg_post_event],
        })
    }
}

#[cfg(not(target_os = "macos"))]
impl TccOps for SystemTcc {
    fn prior(&self, _permission: Permission) -> Prior {
        Prior::Unknowable
    }

    fn open_pane(&mut self, _permission: Permission) {}

    fn open_pane_unless_granted(&mut self, _permission: Permission, _after: Duration) {}

    fn reset(&mut self, _permission: Permission) -> Result<(), String> {
        Err("this OS has no permission entries to reset".into())
    }

    fn raw(&self) -> Option<Raw> {
        None
    }
}

/// Does nothing at all: what the agent's own tests run with, so no test can show a prompt, open
/// System Settings or reset anything.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct NoTcc;

#[cfg(test)]
impl TccOps for NoTcc {
    fn prior(&self, _permission: Permission) -> Prior {
        Prior::Unknowable
    }
    fn open_pane(&mut self, _permission: Permission) {}
    fn open_pane_unless_granted(&mut self, _permission: Permission, _after: Duration) {}
    fn reset(&mut self, _permission: Permission) -> Result<(), String> {
        Ok(())
    }
    fn raw(&self) -> Option<Raw> {
        None
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::{Arc, Mutex};

    use crosspane_platform::{EventSink, PlatformError};

    use super::*;

    /// The Mac's four grants; what is in `granted` is granted. Requests are only recorded.
    #[derive(Clone, Default)]
    struct FakePerms {
        granted: Arc<Mutex<Vec<Permission>>>,
        requested: Arc<Mutex<Vec<Permission>>>,
        microphone: bool,
    }

    impl Permissions for FakePerms {
        fn required(&self) -> Vec<Permission> {
            let mut r = vec![
                Permission::ScreenRecording,
                Permission::Accessibility,
                Permission::InputMonitoring,
            ];
            if self.microphone {
                r.push(Permission::Microphone);
            }
            r
        }
        fn state(&self, permission: Permission) -> PermissionState {
            if self.granted.lock().unwrap().contains(&permission) {
                PermissionState::Granted
            } else {
                PermissionState::NotGranted
            }
        }
        fn request(&mut self, permission: Permission) -> Result<(), PlatformError> {
            self.requested.lock().unwrap().push(permission);
            Ok(())
        }
        fn subscribe(
            &mut self,
            _sink: Arc<dyn EventSink<(Permission, PermissionState)>>,
        ) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum OpsCall {
        Pane(Permission),
        PaneUnlessGranted(Permission, Duration),
        Reset(Permission),
    }

    #[derive(Default)]
    struct FakeOps {
        priors: Vec<(Permission, Prior)>,
        calls: Vec<OpsCall>,
        reset_fails: bool,
    }

    impl TccOps for FakeOps {
        fn prior(&self, permission: Permission) -> Prior {
            self.priors
                .iter()
                .find(|(p, _)| *p == permission)
                .map_or(Prior::Unknowable, |(_, prior)| *prior)
        }
        fn open_pane(&mut self, permission: Permission) {
            self.calls.push(OpsCall::Pane(permission));
        }
        fn open_pane_unless_granted(&mut self, permission: Permission, after: Duration) {
            self.calls
                .push(OpsCall::PaneUnlessGranted(permission, after));
        }
        fn reset(&mut self, permission: Permission) -> Result<(), String> {
            self.calls.push(OpsCall::Reset(permission));
            if self.reset_fails {
                Err("no".into())
            } else {
                Ok(())
            }
        }
        fn raw(&self) -> Option<Raw> {
            None
        }
    }

    fn requested(perms: &FakePerms) -> Vec<Permission> {
        perms.requested.lock().unwrap().clone()
    }

    #[test]
    fn one_click_asks_exactly_one_permission() {
        let mut perms = FakePerms {
            microphone: true,
            ..FakePerms::default()
        };
        let mut ops = FakeOps::default();
        let mut record = AskRecord::default();
        let asked = ask_first_missing(&mut perms, &mut ops, &mut record).unwrap();
        assert_eq!(asked.permission, Some("accessibility"));
        assert_eq!(asked.shown, Shown::PromptThenPane);
        assert_eq!(requested(&perms), [Permission::Accessibility]);
        assert_eq!(
            ops.calls,
            [OpsCall::PaneUnlessGranted(
                Permission::Accessibility,
                PANE_DELAY
            )]
        );
    }

    #[test]
    fn asks_follow_the_order_and_stop_when_everything_is_granted() {
        let mut perms = FakePerms {
            microphone: true,
            ..FakePerms::default()
        };
        let mut ops = FakeOps::default();
        let mut record = AskRecord::default();
        for (grant, next) in [
            (Permission::Accessibility, Some("input_monitoring")),
            (Permission::InputMonitoring, Some("screen_recording")),
            (Permission::ScreenRecording, Some("microphone")),
            (Permission::Microphone, None),
        ] {
            perms.granted.lock().unwrap().push(grant);
            let asked = ask_first_missing(&mut perms, &mut ops, &mut record).unwrap();
            assert_eq!(asked.permission, next);
        }
        let done = ask_first_missing(&mut perms, &mut ops, &mut record).unwrap();
        assert_eq!(done.shown, Shown::Nothing);
        assert_eq!(done.note, Some("every permission is granted"));
    }

    #[test]
    fn input_monitoring_waits_for_accessibility() {
        let mut perms = FakePerms::default();
        let mut ops = FakeOps::default();
        let mut record = AskRecord::default();
        let asked = ask(
            &mut perms,
            &mut ops,
            &mut record,
            Permission::InputMonitoring,
        )
        .unwrap();
        assert_eq!(asked.permission, Some("accessibility"));
        assert!(asked.note.is_some());
        assert_eq!(requested(&perms), [Permission::Accessibility]);
    }

    #[test]
    fn input_monitoring_is_tri_state() {
        for (prior, shown, panes) in [
            (Prior::NeverAsked, Shown::Prompt, 0),
            (Prior::Answered, Shown::Pane, 1),
        ] {
            let mut perms = FakePerms::default();
            perms
                .granted
                .lock()
                .unwrap()
                .push(Permission::Accessibility);
            let mut ops = FakeOps {
                priors: vec![(Permission::InputMonitoring, prior)],
                ..FakeOps::default()
            };
            let mut record = AskRecord::default();
            let asked = ask(
                &mut perms,
                &mut ops,
                &mut record,
                Permission::InputMonitoring,
            )
            .unwrap();
            assert_eq!(asked.shown, shown);
            // Denied is asked anyway (it re-adds the row), then the pane opens.
            assert_eq!(requested(&perms), [Permission::InputMonitoring]);
            assert_eq!(
                ops.calls
                    .iter()
                    .filter(|c| **c == OpsCall::Pane(Permission::InputMonitoring))
                    .count(),
                panes
            );
        }
    }

    #[test]
    fn microphone_prompts_once_then_opens_its_pane() {
        let mut perms = FakePerms {
            microphone: true,
            ..FakePerms::default()
        };
        let mut ops = FakeOps {
            priors: vec![(Permission::Microphone, Prior::NeverAsked)],
            ..FakeOps::default()
        };
        let mut record = AskRecord::default();
        let first = ask(&mut perms, &mut ops, &mut record, Permission::Microphone).unwrap();
        assert_eq!(first.shown, Shown::Prompt);
        ops.priors = vec![(Permission::Microphone, Prior::Answered)];
        let second = ask(&mut perms, &mut ops, &mut record, Permission::Microphone).unwrap();
        assert_eq!(second.shown, Shown::Pane);
        assert_eq!(requested(&perms), [Permission::Microphone]);
        assert_eq!(ops.calls, [OpsCall::Pane(Permission::Microphone)]);
    }

    #[test]
    fn screen_recording_prompt_is_remembered_across_restarts_and_reset_forgets_it() {
        let dir = std::env::temp_dir().join(format!(
            "crosspane-ask-record-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(ASK_RECORD);
        let _ = std::fs::remove_file(&path);
        let mut perms = FakePerms::default();
        let mut ops = FakeOps::default();
        let mut record = AskRecord::at(Some(path.clone()));
        let first = ask(
            &mut perms,
            &mut ops,
            &mut record,
            Permission::ScreenRecording,
        )
        .unwrap();
        assert_eq!(first.shown, Shown::Prompt);
        // A restarted agent reads the record: the prompt can't show again, so the pane opens.
        let mut record = AskRecord::at(Some(path.clone()));
        let second = ask(
            &mut perms,
            &mut ops,
            &mut record,
            Permission::ScreenRecording,
        )
        .unwrap();
        assert_eq!(second.shown, Shown::Pane);
        assert_eq!(ops.calls, [OpsCall::Pane(Permission::ScreenRecording)]);
        reset(&perms, &mut ops, &mut record, Permission::ScreenRecording).unwrap();
        let mut record = AskRecord::at(Some(path.clone()));
        let third = ask(
            &mut perms,
            &mut ops,
            &mut record,
            Permission::ScreenRecording,
        )
        .unwrap();
        assert_eq!(third.shown, Shown::Prompt);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn granted_or_unneeded_permissions_are_not_asked() {
        let mut perms = FakePerms::default();
        perms
            .granted
            .lock()
            .unwrap()
            .push(Permission::Accessibility);
        let mut ops = FakeOps::default();
        let mut record = AskRecord::default();
        let asked = ask(&mut perms, &mut ops, &mut record, Permission::Accessibility).unwrap();
        assert_eq!(asked.shown, Shown::Nothing);
        assert!(ask(&mut perms, &mut ops, &mut record, Permission::Microphone).is_err());
        assert!(requested(&perms).is_empty());
        assert!(ops.calls.is_empty());
    }

    #[test]
    fn reset_is_only_for_required_permissions_and_reports_failure() {
        let perms = FakePerms::default();
        let mut ops = FakeOps::default();
        let mut record = AskRecord::default();
        assert!(reset(&perms, &mut ops, &mut record, Permission::Microphone).is_err());
        assert!(
            ops.calls.is_empty(),
            "nothing is reset for an unneeded grant"
        );
        ops.reset_fails = true;
        assert!(reset(&perms, &mut ops, &mut record, Permission::Accessibility).is_err());
        assert_eq!(ops.calls, [OpsCall::Reset(Permission::Accessibility)]);
    }

    #[test]
    fn tokens_round_trip() {
        for p in ASK_ORDER {
            assert_eq!(permission_named(permission_token(p)), Ok(p));
        }
        assert!(permission_named("camera").is_err());
    }

    fn reading(granted: &[Permission], required: usize, key: &[bool]) -> Reading {
        Reading {
            granted: granted.to_vec(),
            complete: granted.len() == required,
            backend_key: key.to_vec(),
        }
    }

    #[test]
    fn a_completed_set_restarts_after_one_confirming_poll() {
        let t0 = Instant::now();
        let mut watch = GrantWatch::new(reading(&[Permission::ScreenRecording], 3, &[false]));
        let all = reading(
            &[
                Permission::ScreenRecording,
                Permission::Accessibility,
                Permission::InputMonitoring,
            ],
            3,
            &[false],
        );
        assert_eq!(watch.observe(t0, all.clone()), None);
        assert_eq!(watch.observe(t0 + POLL, all), Some(RestartReason::Complete));
    }

    #[test]
    fn a_partial_change_restarts_once_it_has_settled() {
        let t0 = Instant::now();
        let mut watch = GrantWatch::new(reading(&[], 3, &[false]));
        let one = reading(&[Permission::Accessibility], 3, &[false]);
        assert_eq!(watch.observe(t0, one.clone()), None);
        assert_eq!(
            watch.observe(t0 + Duration::from_secs(5), one.clone()),
            None
        );
        // The person allows the next one: the clock starts again.
        let two = reading(
            &[Permission::Accessibility, Permission::InputMonitoring],
            3,
            &[false],
        );
        assert_eq!(
            watch.observe(t0 + Duration::from_secs(6), two.clone()),
            None
        );
        assert_eq!(
            watch.observe(t0 + Duration::from_secs(15), two.clone()),
            None
        );
        assert_eq!(
            watch.observe(t0 + Duration::from_secs(16), two),
            Some(RestartReason::Settled)
        );
    }

    #[test]
    fn a_change_that_goes_back_or_flaps_never_restarts() {
        let t0 = Instant::now();
        let base = reading(&[], 3, &[false]);
        let mut watch = GrantWatch::new(base.clone());
        let one = reading(&[Permission::Accessibility], 3, &[false]);
        for i in 0..30u64 {
            let r = if i % 2 == 0 {
                one.clone()
            } else {
                base.clone()
            };
            assert_eq!(watch.observe(t0 + Duration::from_secs(i), r), None);
        }
    }

    /// The 11:08 miss: the grant set alone didn't move while a backend's own check (posting
    /// events) did. The watch sees that too, and a revocation as well.
    #[test]
    fn backend_relevant_raw_changes_and_revocations_are_seen() {
        let t0 = Instant::now();
        let mut watch = GrantWatch::new(reading(&[Permission::Accessibility], 3, &[false]));
        let posted = reading(&[Permission::Accessibility], 3, &[true]);
        assert_eq!(watch.observe(t0, posted.clone()), None);
        assert_eq!(
            watch.observe(t0 + SETTLE, posted),
            Some(RestartReason::Settled)
        );
        let full = [
            Permission::ScreenRecording,
            Permission::Accessibility,
            Permission::InputMonitoring,
        ];
        let mut watch = GrantWatch::new(reading(&full, 3, &[true]));
        let revoked = reading(&full[..2], 3, &[true]);
        assert_eq!(watch.observe(t0, revoked.clone()), None);
        assert_eq!(
            watch.observe(t0 + SETTLE, revoked),
            Some(RestartReason::Settled)
        );
    }

    #[test]
    fn a_reading_reflects_the_required_set() {
        let perms = FakePerms {
            microphone: true,
            ..FakePerms::default()
        };
        perms.granted.lock().unwrap().extend([
            Permission::ScreenRecording,
            Permission::Accessibility,
            Permission::InputMonitoring,
        ]);
        let r = Reading::of(&perms, None);
        assert!(!r.complete, "the microphone is still missing");
        perms.granted.lock().unwrap().push(Permission::Microphone);
        assert!(Reading::of(&perms, None).complete);
    }
}
