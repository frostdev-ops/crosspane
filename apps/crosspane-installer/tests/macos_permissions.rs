#![cfg(target_os = "macos")]
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
//! Only explicit temporary resources and injected process/signature/GUI facts. No ctl
//! connection, native command, Settings URL, prompt, audio or owner resource is accessed.
use crosspane_installer::platform::macos::{native_io, transport};
use crosspane_installer::{agent_contract, settings_transition, view};
#[path = "../src/platform/macos/permissions.rs"]
mod permissions;
use agent_contract::*;
use native_io::*;
use permissions::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use transport::SelectedAgent;
use view::HidingChoice;

const LOCAL: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const PEER: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const OLD: &str = "1111111111111111";
const NEW: &str = "2222222222222222";
static NONCE: AtomicU64 = AtomicU64::new(1);
#[derive(Default)]
struct FakeClock(AtomicU64);
impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}
struct FakeSupport(SupportObservation);
impl SupportProbe for FakeSupport {
    fn observe(&self, d: &Deadline) -> NativeResult<SupportObservation> {
        d.check()?;
        Ok(self.0.clone())
    }
}
#[derive(Default)]
struct FakeSignatures {
    requirement: Mutex<Option<String>>,
}
impl SignatureProbe for FakeSignatures {
    fn observe(
        &self,
        _: &Path,
        approved: &SigningRequirement,
        d: &Deadline,
    ) -> NativeResult<SignatureObservation> {
        d.check()?;
        Ok(SignatureObservation {
            strict_verified: true,
            team_identifier: "ABCDE12345".into(),
            identifier: approved.identifier.clone(),
            designated_requirement: self
                .requirement
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| approved.designated_requirement.clone()),
            entitlements: approved.entitlements.clone(),
            apple_development: true,
            hardened_runtime: true,
            ad_hoc: false,
        })
    }
}
struct FakeRunner {
    uid: u32,
    exe: PathBuf,
    calls: Mutex<Vec<Vec<String>>>,
}
impl CommandRunner for FakeRunner {
    fn run(&self, c: &CommandSpec, d: &Deadline) -> NativeResult<CommandOutput> {
        d.check()?;
        // Any attempted native fallback, permission command or service mutation fails the test.
        assert_eq!(c.program(), Path::new("/bin/ps"));
        assert!(!c.is_mutation());
        assert_eq!(c.environment()["LC_ALL"], "C");
        assert_eq!(c.environment()["TZ"], "UTC");
        self.calls.lock().unwrap().push(c.args().to_vec());
        let text = match c.args()[1].as_str() {
            "uid=" => self.uid.to_string(),
            "lstart=" => "Thu Jan  1 00:00:00 1970".into(),
            "comm=" => self.exe.to_string_lossy().into_owned(),
            _ => panic!("unapproved fake command"),
        };
        Ok(CommandOutput {
            code: Some(0),
            stdout: format!("{text}\n").into_bytes(),
            stderr: vec![],
        })
    }
}
struct Fixture {
    root: PathBuf,
    io: Arc<MacNativeIo>,
    clock: Arc<FakeClock>,
    signatures: Arc<FakeSignatures>,
    runner: Arc<FakeRunner>,
    listener: UnixListener,
    instance: AtomicU64,
}
fn directory(p: &Path) {
    fs::create_dir_all(p).unwrap();
    fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
}
fn bytes(p: &Path, b: &[u8], mode: u32) {
    directory(p.parent().unwrap());
    fs::write(p, b).unwrap();
    fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
}
impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(format!(
            "/private/tmp/cp-guide-{}-{}",
            std::process::id(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let (home, tmp) = (root.join("h"), root.join("t"));
        directory(&home);
        directory(&tmp);
        directory(&tmp.join("crosspane"));
        directory(&tmp.join("payload"));
        let uid = rustix::process::geteuid().as_raw();
        let target = MacTarget::scratch(TargetPaths {
            uid,
            home,
            gui_tmpdir: tmp.clone(),
            runtime_override: None,
            payload_root: tmp.join("payload"),
        })
        .unwrap();
        bytes(&target.agent_path(), b"signed executable fixture", 0o755);
        let listener = UnixListener::bind(target.socket_path()).unwrap();
        fs::set_permissions(target.socket_path(), fs::Permissions::from_mode(0o600)).unwrap();
        let clock = Arc::new(FakeClock::default());
        let signatures = Arc::new(FakeSignatures::default());
        let runner = Arc::new(FakeRunner {
            uid,
            exe: target.agent_path(),
            calls: Mutex::default(),
        });
        let support = Arc::new(FakeSupport(SupportObservation {
            macos_major: 26,
            apple_silicon: true,
            gui: GuiObservation {
                console_uid: Some(uid),
                interactive_uid: Some(uid),
                console_session: "fixture-aqua".into(),
                interactive_session: "fixture-aqua".into(),
                active: true,
            },
            gui_tmpdir: tmp,
        }));
        let io = Arc::new(
            MacNativeIo::new(
                target,
                runner.clone(),
                support,
                signatures.clone(),
                clock.clone(),
            )
            .unwrap(),
        );
        let f = Self {
            root,
            io,
            clock,
            signatures,
            runner,
            listener,
            instance: AtomicU64::new(1),
        };
        f.bootstrap(1, "ready");
        f
    }
    fn now(&self) -> u64 {
        self.clock.now_ms()
    }
    fn advance(&self) {
        self.clock.0.fetch_add(1, Ordering::Relaxed);
    }
    fn deadline(&self) -> Deadline {
        Deadline::new(5000, self.clock.clone(), Cancellation::default()).unwrap()
    }
    fn main(&self) -> SignatureProof {
        self.io
            .admit_main_signature(
                &self.io.target().agent_path(),
                &SigningRequirement {
                    role: ArtifactRole::Agent,
                    identifier: AGENT_LABEL.into(),
                    designated_requirement: "approved development fixture".into(),
                    entitlements: BTreeMap::new(),
                },
                &self.deadline(),
            )
            .unwrap()
    }
    fn bootstrap(&self, id: u64, phase: &str) {
        self.instance.store(id, Ordering::Relaxed);
        let b = json!({"schema_version":1,"instance_id":id,"pid":4242,"started_unix_ms":0,"phase":phase,"phase_seq":1,"keystore":null,"reason":null,"runtime_dir":self.io.target().runtime_dir()});
        bytes(
            &self.io.target().runtime_dir().join("bootstrap.json"),
            &serde_json::to_vec(&b).unwrap(),
            0o600,
        );
    }
    fn admission(&self) -> GuideAdmission {
        let main = self.main();
        let support = self.io.admit_support(&main, &self.deadline()).unwrap();
        let instance = Arc::new(
            self.io
                .admit_instance(&support, &main, &self.deadline())
                .unwrap(),
        );
        let selected = SelectedAgent {
            io: self.io.clone(),
            support,
            instance,
            link: None,
        };
        let mut admission = GuideAdmission::ready(&selected, &main, &self.deadline()).unwrap();
        // Explicitly simulate a Live transport receipt, never an actual owner runtime.
        admission.emulate_live();
        admission
    }
    fn raw(&self) -> Value {
        let names = [
            "capture",
            "keys",
            "pointer",
            "overlay",
            "hotkeys",
            "keystore",
            "windows",
            "parking",
            "frames",
            "tray",
            "links",
            "gpu",
            "home",
            "audio",
            "discovery",
        ];
        let backends: Vec<_> = names
            .into_iter()
            .map(|name| json!({"name":name,"state":"ready","reason":null}))
            .collect();
        let counters = json!({"e1_controller_started":0,"e1_controller_ended":0,"e1_target_started":0,"e1_target_ended":0,"e1_injections_ok":0,"e1_hud_shows":0,"e1_chord_releases":0,"e1_command_releases":0,"e2_source_started":0,"e2_source_returned":0,"e2_dest_started":0,"e2_dest_returned":0,"e2_frames_presented":null,"e2_returns_failed":0});
        json!({"ok":true,"result":{"controlling":null,"controlled_by":null,"projections":[],"displays":[],"peers":[],"layout":[],"installer":{"schema_version":1,"build":{"version":"fixture","features":["private-vdisplay","video"]},"instance":{"id":self.instance.load(Ordering::Relaxed),"pid":4242,"uid":self.runner.uid,"exe":self.runner.exe,"runtime_dir":self.io.target().runtime_dir(),"started_unix_ms":0},"config_revision":OLD,"node":LOCAL,"recovery_pending":0,"startup_recovery":"restored","gate":{"open":true,"session":"unlocked","active":true,"armed":true,"panic":false},"epochs":{"gate":1,"grants":1,"layout":1,"backends":1},"keystore":"os_store","permissions":[{"name":"screen_recording","state":"granted"},{"name":"accessibility","state":"granted"},{"name":"input_monitoring","state":"granted"},{"name":"microphone","state":"granted"}],"backends":backends,"discovery":{"enabled":true,"running":true,"candidates":0,"error":null},"tray":{"created":true},"audio":{"enabled":true,"active_peers":[],"frames_sent":0,"frames_played":0},"settings_opened":0,"peers":[{"node":PEER,"name":"Other fixture","connected":false,"link_generation":null,"features":[],"grants_given":[],"last_source_parking":null,"counters":counters}]}}})
    }
    fn guide(&self) -> PermissionGuide {
        PermissionGuide::new(self.admission(), LOCAL.parse().unwrap(), self.now(), 1).unwrap()
    }
    fn answer(
        &self,
        g: &mut PermissionGuide,
        effects: Vec<GuideIntent>,
        result: Result<DecodedReply, CallFailure>,
    ) -> Vec<GuideIntent> {
        self.answer_source(g, effects, result, ObservationSource::Live)
    }
    fn answer_source(
        &self,
        g: &mut PermissionGuide,
        effects: Vec<GuideIntent>,
        result: Result<DecodedReply, CallFailure>,
        source: ObservationSource,
    ) -> Vec<GuideIntent> {
        let (binding, call) = agent(effects);
        self.advance();
        g.reply(
            &binding,
            AgentReply {
                id: call.id,
                observed_at_ms: self.now(),
                source,
                result,
            },
            self.now(),
        )
        .unwrap()
    }
    fn poll(&self, g: &mut PermissionGuide, raw: Value) -> Vec<GuideIntent> {
        let effects = g.poll_health(self.now()).unwrap();
        let status =
            parse_status(&serde_json::to_vec(&raw).unwrap(), AgentPlatform::Macos).unwrap();
        // Frozen Mac transport marks real pending-contract replies unadmitted/Demo.
        let source = if matches!(status, StatusAdmission::PendingHealthContract(_)) {
            ObservationSource::Demo
        } else {
            ObservationSource::Live
        };
        self.answer_source(g, effects, Ok(DecodedReply::Status(status)), source)
    }
    fn loaded(&self) -> PermissionGuide {
        let mut g = self.guide();
        self.poll(&mut g, self.raw());
        g
    }
    fn updated(&self, choice: HidingChoice) -> PermissionGuide {
        let mut g = self.loaded();
        let token = g.view(self.now()).token;
        g.choose_hiding(&token, choice, self.now()).unwrap();
        let token = g.view(self.now()).token;
        let effects = g.commit_hiding(&token, self.now()).unwrap();
        assert_eq!(
            agent(effects.clone()).1.request,
            InstallerRequest::SettingsUpdate {
                expected_revision: OLD.into(),
                mac_virtual_display: choice == HidingChoice::Hide
            }
        );
        self.answer(
            &mut g,
            effects,
            Ok(DecodedReply::SettingsUpdated(SettingsUpdated {
                revision: NEW.into(),
                restart_required: true,
            })),
        );
        assert_eq!(
            g.settings_state(),
            &settings_transition::SettingsTransitionState::NeedsRestartConsent
        );
        g
    }
    fn applied(&self, choice: HidingChoice) -> PermissionGuide {
        let mut g = self.updated(choice);
        let token = g.view(self.now()).token;
        let effects = g.restart(&token, true, self.now()).unwrap();
        self.answer(&mut g, effects, Ok(DecodedReply::Acknowledged));
        self.advance();
        self.bootstrap(2, "ready");
        g.redetected(self.admission(), self.now()).unwrap();
        let mut raw = self.raw();
        raw["result"]["installer"]["config_revision"] = json!(NEW);
        self.poll(&mut g, raw);
        g
    }
    fn completed(&self) -> (PermissionGuide, Value) {
        let mut g = self.applied(HidingChoice::Hide);
        let mut raw = self.raw();
        raw["result"]["installer"]["config_revision"] = json!(NEW);
        connected(&mut raw);
        self.poll(&mut g, raw.clone());
        let token = g.view(self.now()).token;
        g.begin_projection_check(&token, PEER.parse().unwrap(), self.now())
            .unwrap();
        raw["result"]["installer"]["peers"][0]["counters"]["e2_source_started"] = json!(1);
        raw["result"]["installer"]["peers"][0]["last_source_parking"] = json!("twin");
        self.poll(&mut g, raw.clone());
        assert_eq!(
            g.confirm_parking(&token, ParkingObservation::Hidden, self.now())
                .unwrap(),
            ParkingVerification::Twin
        );
        (g, raw)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
fn agent(effects: Vec<GuideIntent>) -> (GuideBinding, AgentCall) {
    effects
        .into_iter()
        .find_map(|e| match e {
            GuideIntent::Agent { binding, call } => Some((binding, call)),
            _ => None,
        })
        .expect("one correlated agent intent")
}
fn connected(raw: &mut Value) {
    raw["result"]["installer"]["peers"][0]["connected"] = json!(true);
    raw["result"]["installer"]["peers"][0]["link_generation"] = json!(1);
}

#[test]
fn disabled_audio_three_grants_is_valid_but_audio_prerequisite_is_false() {
    let f = Fixture::new();
    let mut g = f.guide();
    let mut raw = f.raw();
    raw["result"]["installer"]["audio"]["enabled"] = json!(false);
    raw["result"]["installer"]["permissions"]
        .as_array_mut()
        .unwrap()
        .pop();
    f.poll(&mut g, raw);
    let v = g.view(f.now());
    assert!(!v.audio_prerequisite);
    assert_eq!(v.rows.iter().filter(|r| r.required).count(), 3);
    assert_eq!(v.rows[3].state, PermissionState::Unknown);
}
#[test]
fn enabled_four_grants_and_literal_loopback_copy_are_mapped() {
    let f = Fixture::new();
    let g = f.loaded();
    let v = g.view(f.now());
    assert!(v.audio_prerequisite);
    assert!(
        v.rows
            .iter()
            .all(|r| r.required && r.state == PermissionState::Granted)
    );
    assert_eq!(
        MICROPHONE_REASON,
        "lets Crosspane hear the Crosspane speakers device; your real microphone is never opened"
    );
    assert!(MICROPHONE_DETAIL.contains("mic peer capability remains unavailable"));
    assert_eq!(
        HIDE_LABEL,
        "Hide projected windows on a virtual display (recommended)"
    );
    assert_eq!(
        MIRROR_LABEL,
        "Mirror instead (windows stay visible on this Mac)"
    );
    assert!(HIDE_EXPLANATION.contains("Apple private interface"));
}
#[test]
fn enabled_audio_without_fourth_grant_stays_pending() {
    let f = Fixture::new();
    let mut g = f.guide();
    let mut raw = f.raw();
    raw["result"]["installer"]["permissions"]
        .as_array_mut()
        .unwrap()
        .pop();
    f.poll(&mut g, raw);
    assert!(!g.view(f.now()).audio_prerequisite);
    assert!(g.view(f.now()).rows[3].required);
}
#[test]
fn ask_is_multi_dialog_agent_request_and_acknowledgement_is_not_grant() {
    let f = Fixture::new();
    let mut g = f.guide();
    let mut raw = f.raw();
    for row in raw["result"]["installer"]["permissions"]
        .as_array_mut()
        .unwrap()
    {
        row["state"] = json!("not_granted");
    }
    f.poll(&mut g, raw);
    let token = g.view(f.now()).token;
    let effects = g.ask(&token, f.now()).unwrap();
    assert_eq!(
        agent(effects.clone()).1.request,
        InstallerRequest::AskPermissions
    );
    f.answer(&mut g, effects, Ok(DecodedReply::Acknowledged));
    assert!(
        g.view(f.now())
            .rows
            .iter()
            .all(|r| r.state == PermissionState::NotGranted)
    );
    assert_eq!(g.view(f.now()).reason, Some(GuideReason::RequestStarted));
    // WP-4.33: one permission per click, never a burst of dialogs.
    assert!(ASK_EXPLANATION.contains("one permission at a time"));
    assert!(!ASK_EXPLANATION.contains("Several system dialogs"));
    assert_eq!(g.ask(&token, f.now()).unwrap_err(), GuideError::Stale);
}
#[test]
fn old_missing_unknown_contract_and_unknown_grant_never_pass() {
    let f = Fixture::new();
    let mut g = f.loaded();
    for change in 0..3 {
        let mut raw = f.raw();
        match change {
            0 => raw["result"]["installer"]["schema_version"] = json!(9),
            1 => {
                raw["result"]["installer"]
                    .as_object_mut()
                    .unwrap()
                    .remove("permissions");
            }
            _ => raw["result"]["installer"]["startup_recovery"] = json!("unknown"),
        };
        f.poll(&mut g, raw);
        assert!(!g.view(f.now()).audio_prerequisite);
        assert!(
            g.view(f.now())
                .rows
                .iter()
                .all(|r| r.state == PermissionState::Unknown)
        );
    }
    let mut raw = f.raw();
    raw["result"]["installer"]["permissions"][0]["state"] = json!("unknown");
    f.poll(&mut g, raw);
    assert!(!g.view(f.now()).audio_prerequisite);
}
#[test]
fn wrong_target_signing_instance_and_demo_receipts_are_rejected() {
    let f = Fixture::new();
    let other = Fixture::new();
    let mut g = f.loaded();
    other.clock.0.store(f.now(), Ordering::Relaxed);
    assert!(
        g.redetected(other.admission(), f.now())
            .unwrap()
            .iter()
            .any(|e| matches!(e, GuideIntent::RetireActivity { .. }))
    );
    assert_eq!(g.view(f.now()).reason, Some(GuideReason::MigrationPending));
    assert!(!g.view(f.now()).audio_prerequisite);
    let mut g = f.loaded();
    let effects = g.poll_health(f.now()).unwrap();
    let (binding, call) = agent(effects);
    let mut raw = f.raw();
    raw["result"]["installer"]["instance"]["id"] = json!(7);
    assert_eq!(
        g.reply(
            &binding,
            AgentReply {
                id: call.id,
                observed_at_ms: f.now(),
                source: ObservationSource::Live,
                result: Ok(DecodedReply::Status(
                    parse_status(&serde_json::to_vec(&raw).unwrap(), AgentPlatform::Macos).unwrap()
                ))
            },
            f.now()
        )
        .unwrap_err(),
        GuideError::Invalid
    );
    assert_eq!(
        g.reply(
            &binding,
            AgentReply {
                id: call.id,
                observed_at_ms: f.now(),
                source: ObservationSource::Demo,
                result: Ok(DecodedReply::Status(
                    parse_status(&serde_json::to_vec(&f.raw()).unwrap(), AgentPlatform::Macos)
                        .unwrap()
                ))
            },
            f.now()
        )
        .unwrap_err(),
        GuideError::Stale
    );
    *f.signatures.requirement.lock().unwrap() = Some("different identity".into());
    assert_eq!(
        f.io.admit_main_signature(
            &f.io.target().agent_path(),
            &SigningRequirement {
                role: ArtifactRole::Agent,
                identifier: AGENT_LABEL.into(),
                designated_requirement: "approved development fixture".into(),
                entitlements: BTreeMap::new()
            },
            &f.deadline()
        )
        .unwrap_err(),
        NativeError::Unsupported
    );
}
#[test]
fn open_pane_has_exact_enum_allowlist_and_failure_breadcrumbs() {
    let f = Fixture::new();
    let mut g = f.loaded();
    for (pane, suffix) in [
        (SettingsPane::ScreenRecording, "Privacy_ScreenCapture"),
        (SettingsPane::Accessibility, "Privacy_Accessibility"),
        (SettingsPane::InputMonitoring, "Privacy_ListenEvent"),
        (SettingsPane::Microphone, "Privacy_Microphone"),
    ] {
        let token = g.view(f.now()).token;
        g.select_permission(&token, pane, f.now()).unwrap();
        let token = g.view(f.now()).token;
        let effects = g.open_pane(&token, f.now()).unwrap();
        let GuideIntent::OpenPane {
            binding,
            pane: selected,
        } = &effects[0]
        else {
            panic!("wrong intent")
        };
        assert_eq!(*selected, pane);
        assert_eq!(
            pane.url(),
            format!("x-apple.systempreferences:com.apple.preference.security?{suffix}")
        );
        let calls = f.runner.calls.lock().unwrap().len();
        g.pane_completed(binding, Err(NativeError::Unavailable), f.now())
            .unwrap();
        assert_eq!(f.runner.calls.lock().unwrap().len(), calls);
        assert_eq!(g.view(f.now()).breadcrumbs, Some(pane.breadcrumbs()));
        assert_eq!(g.open_pane(&token, f.now()).unwrap_err(), GuideError::Stale);
    }
}
#[test]
fn stale_preflight_explicit_restart_waits_for_confirmed_new_instance_and_recovery() {
    let f = Fixture::new();
    let mut g = f.guide();
    let mut raw = f.raw();
    raw["result"]["installer"]["permissions"][0]["state"] = json!("not_granted");
    f.poll(&mut g, raw.clone());
    let token = g.view(f.now()).token;
    let effects = g.restart(&token, true, f.now()).unwrap();
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, GuideIntent::RetireActivity { .. }))
    );
    f.answer(&mut g, effects, Ok(DecodedReply::Acknowledged));
    assert!(
        g.poll_health(f.now())
            .unwrap()
            .iter()
            .all(|e| matches!(e, GuideIntent::Redetect { .. }))
    );
    assert!(!g.view(f.now()).audio_prerequisite);
    f.advance();
    g.redetected(f.admission(), f.now()).unwrap();
    assert!(
        g.poll_health(f.now())
            .unwrap()
            .iter()
            .all(|e| matches!(e, GuideIntent::Redetect { .. }))
    );
    f.advance();
    f.bootstrap(2, "ready");
    let invalidation = g.redetected(f.admission(), f.now()).unwrap();
    assert!(
        invalidation
            .iter()
            .any(|e| matches!(e, GuideIntent::RetireActivity { .. }))
    );
    f.poll(&mut g, f.raw());
    assert!(g.view(f.now()).audio_prerequisite);
    f.advance();
    f.bootstrap(1, "ready");
    assert_eq!(
        g.redetected(f.admission(), f.now()).unwrap_err(),
        GuideError::Stale
    );
}
#[test]
fn active_restart_requires_explained_interruption_and_retired_view_cannot_restart() {
    let f = Fixture::new();
    let mut g = f.guide();
    let mut raw = f.raw();
    raw["result"]["controlling"] = json!(PEER);
    f.poll(&mut g, raw);
    let token = g.view(f.now()).token;
    assert!(
        g.view(f.now())
            .restart_warning
            .contains("ends active control")
    );
    assert_eq!(
        g.restart(&token, false, f.now()).unwrap_err(),
        GuideError::NotReady
    );
    let effects = g.restart(&token, true, f.now()).unwrap();
    assert_eq!(agent(effects).1.request, InstallerRequest::Restart);
    assert_eq!(
        g.restart(&token, true, f.now()).unwrap_err(),
        GuideError::Stale
    );
}
#[test]
fn timers_only_poll_and_timeout_never_auto_asks_or_restarts() {
    let f = Fixture::new();
    let mut g = f.loaded();
    assert!(g.tick(f.now()).unwrap().is_empty());
    for _ in 0..3 {
        let effects = g.poll_health(f.now()).unwrap();
        assert_eq!(agent(effects.clone()).1.request, InstallerRequest::Status);
        f.answer(
            &mut g,
            effects,
            Ok(DecodedReply::Status(
                parse_status(&serde_json::to_vec(&f.raw()).unwrap(), AgentPlatform::Macos).unwrap(),
            )),
        );
    }
    let token = g.view(f.now()).token;
    let effects = g.restart(&token, true, f.now()).unwrap();
    let (_, old_call) = agent(effects);
    f.clock.0.fetch_add(5000, Ordering::Relaxed);
    let effects = g.tick(f.now()).unwrap();
    assert!(
        effects
            .iter()
            .all(|e| !matches!(e, GuideIntent::Agent { .. }))
    );
    assert_eq!(g.view(f.now()).reason, Some(GuideReason::OutcomeUnknown));
    assert!(g.tick(f.now()).unwrap().is_empty());
    let token = g.view(f.now()).token;
    assert!(g.restart(&token, true, f.now()).is_err());
    assert!(old_call.id > 0);
}
#[test]
fn keychain_waiting_bootstrap_has_no_ctl_or_fallback_action() {
    let f = Fixture::new();
    f.bootstrap(1, "waiting_for_keystore");
    let main = f.main();
    let support = f.io.admit_support(&main, &f.deadline()).unwrap();
    let mut a = GuideAdmission::waiting(&f.io, &support, &main, &f.deadline()).unwrap();
    a.emulate_live();
    let mut g = PermissionGuide::new(a, LOCAL.parse().unwrap(), f.now(), 1).unwrap();
    assert_eq!(g.view(f.now()).reason, Some(GuideReason::KeychainWaiting));
    assert!(
        g.poll_health(f.now())
            .unwrap()
            .iter()
            .all(|e| matches!(e, GuideIntent::Redetect { .. }))
    );
    let token = g.view(f.now()).token;
    assert_eq!(g.ask(&token, f.now()).unwrap_err(), GuideError::NotReady);
    assert_eq!(
        g.restart(&token, true, f.now()).unwrap_err(),
        GuideError::NotReady
    );
}
#[test]
fn discovery_empty_unknown_and_reported_denial_are_distinct_from_connectivity() {
    let f = Fixture::new();
    let mut g = f.loaded();
    let addr = "192.0.2.1:47811".parse().unwrap();
    let token = g.view(f.now()).token;
    assert_eq!(
        g.dial(&token, addr, f.now()).unwrap_err(),
        GuideError::NotReady
    );
    g.explain_network(&token, f.now()).unwrap();
    assert_eq!(g.view(f.now()).network, NetworkState::WaitingForPeer);
    assert!(NETWORK_EXPLANATION.contains("empty search"));
    let mut raw = f.raw();
    raw["result"]["installer"]["discovery"]["error"] = json!("unknown");
    f.poll(&mut g, raw);
    assert_eq!(g.view(f.now()).network, NetworkState::UnknownConnectivity);
    let token = g.view(f.now()).token;
    g.report_network_denial(&token, f.now()).unwrap();
    assert_eq!(g.view(f.now()).network, NetworkState::ReportedDenied);
    assert!(
        g.view(f.now())
            .breadcrumbs
            .unwrap()
            .contains("Local Network")
    );
    let token = g.view(f.now()).token;
    let effects = g.dial(&token, addr, f.now()).unwrap();
    assert_eq!(
        agent(effects.clone()).1.request,
        InstallerRequest::Dial { addr }
    );
    f.answer(&mut g, effects, Ok(DecodedReply::Acknowledged));
    assert_eq!(g.view(f.now()).network, NetworkState::UnknownConnectivity);
    let mut raw = f.raw();
    connected(&mut raw);
    f.poll(&mut g, raw);
    assert_eq!(g.view(f.now()).network, NetworkState::Connected);
}
#[test]
fn d7_neither_selected_requires_current_consent_and_restart_required_reply() {
    let f = Fixture::new();
    let mut g = f.loaded();
    assert_eq!(g.view(f.now()).hiding_choice, None);
    assert!(!g.view(f.now()).hiding_next_enabled);
    let token = g.view(f.now()).token;
    assert_eq!(
        g.commit_hiding(&token, f.now()).unwrap_err(),
        GuideError::NotReady
    );
    g.choose_hiding(&token, HidingChoice::Hide, f.now())
        .unwrap();
    assert!(g.view(f.now()).hiding_next_enabled);
    assert_eq!(
        g.commit_hiding(&token, f.now()).unwrap_err(),
        GuideError::Stale
    );
    let token = g.view(f.now()).token;
    let effects = g.commit_hiding(&token, f.now()).unwrap();
    let (binding, call) = agent(effects);
    f.advance();
    assert_eq!(
        g.reply(
            &binding,
            AgentReply {
                id: call.id,
                observed_at_ms: f.now(),
                source: ObservationSource::Live,
                result: Ok(DecodedReply::SettingsUpdated(SettingsUpdated {
                    revision: NEW.into(),
                    restart_required: false
                }))
            },
            f.now()
        )
        .unwrap_err(),
        GuideError::Invalid
    );
}
#[test]
fn external_disk_edit_conflict_requires_reobserve_recovery_restart_and_new_consent() {
    let f = Fixture::new();
    let mut g = f.loaded();
    let token = g.view(f.now()).token;
    g.choose_hiding(&token, HidingChoice::Hide, f.now())
        .unwrap();
    let token = g.view(f.now()).token;
    let effects = g.commit_hiding(&token, f.now()).unwrap();
    assert_eq!(
        agent(effects.clone()).1.request,
        InstallerRequest::SettingsUpdate {
            expected_revision: OLD.into(),
            mac_virtual_display: true
        }
    );
    f.answer(
        &mut g,
        effects,
        Err(CallFailure::Refused(AgentRefusal::RevisionConflict)),
    );
    assert_eq!(
        g.commit_hiding(&token, f.now()).unwrap_err(),
        GuideError::Stale
    );
    f.advance();
    g.redetected(f.admission(), f.now()).unwrap();
    f.poll(&mut g, f.raw());
    let token = g.view(f.now()).token;
    g.redetect_hiding(&token, f.now()).unwrap();
    let token = g.view(f.now()).token;
    assert!(g.commit_hiding(&token, f.now()).is_err());
    let effects = g.restart(&token, true, f.now()).unwrap();
    f.answer(&mut g, effects, Ok(DecodedReply::Acknowledged));
    f.advance();
    f.bootstrap(2, "ready");
    g.redetected(f.admission(), f.now()).unwrap();
    let mut raw = f.raw();
    raw["result"]["installer"]["config_revision"] = json!(NEW);
    f.poll(&mut g, raw);
    let token = g.view(f.now()).token;
    g.redetect_hiding(&token, f.now()).unwrap();
    let token = g.view(f.now()).token;
    let effects = g.commit_hiding(&token, f.now()).unwrap();
    assert_eq!(
        agent(effects).1.request,
        InstallerRequest::SettingsUpdate {
            expected_revision: NEW.into(),
            mac_virtual_display: true
        }
    );
}
#[test]
fn loaded_revision_and_old_instance_cannot_prove_returned_settings_revision() {
    let f = Fixture::new();
    let mut g = f.updated(HidingChoice::Hide);
    assert_eq!(g.view(f.now()).parking, ParkingVerification::Pending);
    let token = g.view(f.now()).token;
    let effects = g.restart(&token, true, f.now()).unwrap();
    f.answer(&mut g, effects, Ok(DecodedReply::Acknowledged));
    f.advance();
    g.redetected(f.admission(), f.now()).unwrap();
    assert!(
        g.poll_health(f.now())
            .unwrap()
            .iter()
            .all(|e| matches!(e, GuideIntent::Redetect { .. }))
    );
    assert!(
        g.begin_projection_check(&g.view(f.now()).token, PEER.parse().unwrap(), f.now())
            .is_err()
    );
    f.advance();
    f.bootstrap(2, "ready");
    g.redetected(f.admission(), f.now()).unwrap();
    let mut raw = f.raw();
    connected(&mut raw);
    f.poll(&mut g, raw);
    assert!(
        g.begin_projection_check(&g.view(f.now()).token, PEER.parse().unwrap(), f.now())
            .is_err()
    );
    let mut raw = f.raw();
    connected(&mut raw);
    raw["result"]["installer"]["config_revision"] = json!(NEW);
    f.poll(&mut g, raw);
    assert!(
        g.begin_projection_check(&g.view(f.now()).token, PEER.parse().unwrap(), f.now())
            .is_ok()
    );
}
#[test]
fn failed_or_unknown_startup_recovery_at_zero_count_stays_pending() {
    let f = Fixture::new();
    let mut g = f.applied(HidingChoice::Hide);
    for recovery in ["failed", "none"] {
        let mut raw = f.raw();
        raw["result"]["installer"]["config_revision"] = json!(NEW);
        raw["result"]["installer"]["startup_recovery"] = json!(recovery);
        connected(&mut raw);
        f.poll(&mut g, raw);
        assert_eq!(g.view(f.now()).reason, Some(GuideReason::RecoveryPending));
        assert!(!g.view(f.now()).audio_prerequisite);
        assert_eq!(
            g.begin_projection_check(&g.view(f.now()).token, PEER.parse().unwrap(), f.now())
                .unwrap_err(),
            GuideError::NotReady
        );
    }
}
#[test]
fn projection_verification_requires_fresh_counter_mode_and_human_hidden_or_mirror() {
    for (choice, mode, human, expected) in [
        (
            HidingChoice::Hide,
            "twin",
            ParkingObservation::Hidden,
            ParkingVerification::Twin,
        ),
        (
            HidingChoice::Hide,
            "mirror",
            ParkingObservation::Visible,
            ParkingVerification::MirrorFallback,
        ),
        (
            HidingChoice::Mirror,
            "mirror",
            ParkingObservation::Visible,
            ParkingVerification::Mirror,
        ),
    ] {
        let f = Fixture::new();
        let mut g = f.applied(choice);
        let mut raw = f.raw();
        raw["result"]["installer"]["config_revision"] = json!(NEW);
        connected(&mut raw);
        f.poll(&mut g, raw.clone());
        let token = g.view(f.now()).token;
        g.begin_projection_check(&token, PEER.parse().unwrap(), f.now())
            .unwrap();
        f.poll(&mut g, raw.clone());
        assert_eq!(
            g.confirm_parking(&token, human, f.now()).unwrap(),
            ParkingVerification::Pending
        );
        raw["result"]["installer"]["peers"][0]["counters"]["e2_source_started"] = json!(1);
        f.poll(&mut g, raw.clone());
        assert_eq!(
            g.confirm_parking(&token, human, f.now()).unwrap(),
            ParkingVerification::Pending
        );
        raw["result"]["installer"]["peers"][0]["last_source_parking"] = json!(mode);
        f.poll(&mut g, raw);
        assert_eq!(g.confirm_parking(&token, human, f.now()).unwrap(), expected);
        assert_eq!(g.view(f.now()).parking, expected);
    }
}
#[test]
fn projection_epoch_link_change_and_ambiguous_counter_retire_evidence() {
    for change in 0..3 {
        let f = Fixture::new();
        let mut g = f.applied(HidingChoice::Hide);
        let mut raw = f.raw();
        raw["result"]["installer"]["config_revision"] = json!(NEW);
        connected(&mut raw);
        f.poll(&mut g, raw.clone());
        let token = g.view(f.now()).token;
        g.begin_projection_check(&token, PEER.parse().unwrap(), f.now())
            .unwrap();
        raw["result"]["installer"]["peers"][0]["last_source_parking"] = json!("twin");
        raw["result"]["installer"]["peers"][0]["counters"]["e2_source_started"] = json!(1);
        match change {
            0 => raw["result"]["installer"]["epochs"]["backends"] = json!(2),
            1 => raw["result"]["installer"]["peers"][0]["link_generation"] = json!(2),
            _ => raw["result"]["installer"]["peers"][0]["counters"]["e2_source_started"] = json!(2),
        };
        f.poll(&mut g, raw);
        assert_ne!(
            g.confirm_parking(&token, ParkingObservation::Hidden, f.now())
                .ok(),
            Some(ParkingVerification::Twin)
        );
    }
}
#[test]
fn restart_socket_close_before_ack_is_unknown_until_native_new_instance() {
    let f = Fixture::new();
    let mut g = f.updated(HidingChoice::Hide);
    let token = g.view(f.now()).token;
    let effects = g.restart(&token, true, f.now()).unwrap();
    f.answer(&mut g, effects, Err(CallFailure::TimeoutOutcomeUnknown));
    assert!(!g.view(f.now()).audio_prerequisite);
    f.advance();
    f.bootstrap(2, "ready");
    g.redetected(f.admission(), f.now()).unwrap();
    let mut raw = f.raw();
    raw["result"]["installer"]["config_revision"] = json!(NEW);
    f.poll(&mut g, raw);
    assert!(g.view(f.now()).audio_prerequisite);
}
#[test]
fn instance_change_before_restart_reply_retires_old_call_and_keeps_revision_verification() {
    let f = Fixture::new();
    let mut g = f.updated(HidingChoice::Hide);
    let token = g.view(f.now()).token;
    let effects = g.restart(&token, true, f.now()).unwrap();
    let (old_binding, call) = agent(effects);
    f.advance();
    f.bootstrap(2, "ready");
    g.redetected(f.admission(), f.now()).unwrap();
    assert_eq!(
        g.reply(
            &old_binding,
            AgentReply {
                id: call.id,
                observed_at_ms: f.now(),
                source: ObservationSource::Live,
                result: Ok(DecodedReply::Acknowledged)
            },
            f.now()
        )
        .unwrap_err(),
        GuideError::Stale
    );
    let mut raw = f.raw();
    raw["result"]["installer"]["config_revision"] = json!(NEW);
    f.poll(&mut g, raw);
    assert!(g.view(f.now()).audio_prerequisite);
}
#[test]
fn redetection_does_not_extend_old_health_freshness_or_keep_completed_proofs() {
    let f = Fixture::new();
    let mut g = f.loaded();
    assert!(g.view(f.now()).audio_prerequisite);
    f.advance();
    g.redetected(f.admission(), f.now()).unwrap();
    assert!(!g.view(f.now()).audio_prerequisite);
    assert!(
        g.view(f.now())
            .rows
            .iter()
            .all(|r| r.state == PermissionState::Unknown)
    );
    assert_eq!(
        g.view(f.now() + 5001).reason,
        Some(GuideReason::HealthPending)
    );
}

#[test]
fn completed_parking_proof_retires_on_later_link_mode_or_counter_change() {
    for change in 0..3 {
        let f = Fixture::new();
        let mut g = f.applied(HidingChoice::Hide);
        let mut raw = f.raw();
        raw["result"]["installer"]["config_revision"] = json!(NEW);
        connected(&mut raw);
        f.poll(&mut g, raw.clone());
        let token = g.view(f.now()).token;
        g.begin_projection_check(&token, PEER.parse().unwrap(), f.now())
            .unwrap();
        raw["result"]["installer"]["peers"][0]["counters"]["e2_source_started"] = json!(1);
        raw["result"]["installer"]["peers"][0]["last_source_parking"] = json!("twin");
        f.poll(&mut g, raw.clone());
        assert_eq!(
            g.confirm_parking(&token, ParkingObservation::Hidden, f.now())
                .unwrap(),
            ParkingVerification::Twin
        );
        assert!(g.view(f.now()).hiding_next_enabled);
        match change {
            0 => raw["result"]["installer"]["peers"][0]["link_generation"] = json!(2),
            1 => raw["result"]["installer"]["peers"][0]["last_source_parking"] = json!("mirror"),
            _ => raw["result"]["installer"]["peers"][0]["counters"]["e2_source_started"] = json!(2),
        }
        f.poll(&mut g, raw);
        assert_eq!(g.view(f.now()).parking, ParkingVerification::Pending);
        assert!(!g.view(f.now()).hiding_next_enabled);
    }
}

#[test]
fn changed_designated_signing_requirement_is_tier_two_pending() {
    let f = Fixture::new();
    let mut g = f.loaded();
    let (binding, call) = agent(g.poll_health(f.now()).unwrap());
    f.advance();
    let requirement = SigningRequirement {
        role: ArtifactRole::Agent,
        identifier: AGENT_LABEL.into(),
        designated_requirement: "other approved signing fixture".into(),
        entitlements: BTreeMap::new(),
    };
    let main =
        f.io.admit_main_signature(&f.io.target().agent_path(), &requirement, &f.deadline())
            .unwrap();
    let support = f.io.admit_support(&main, &f.deadline()).unwrap();
    let instance = Arc::new(f.io.admit_instance(&support, &main, &f.deadline()).unwrap());
    let selected = SelectedAgent {
        io: f.io.clone(),
        support,
        instance,
        link: None,
    };
    let mut admission = GuideAdmission::ready(&selected, &main, &f.deadline()).unwrap();
    admission.emulate_live();
    assert!(
        g.redetected(admission, f.now())
            .unwrap()
            .iter()
            .any(|e| matches!(e, GuideIntent::RetireActivity { .. }))
    );
    assert_eq!(g.view(f.now()).reason, Some(GuideReason::MigrationPending));
    assert_eq!(
        g.reply(
            &binding,
            AgentReply {
                id: call.id,
                observed_at_ms: f.now(),
                source: ObservationSource::Live,
                result: Ok(DecodedReply::Status(
                    parse_status(&serde_json::to_vec(&f.raw()).unwrap(), AgentPlatform::Macos)
                        .unwrap()
                ))
            },
            f.now()
        )
        .unwrap_err(),
        GuideError::Stale
    );
    let token = g.view(f.now()).token;
    assert_eq!(g.ask(&token, f.now()).unwrap_err(), GuideError::NotReady);
    assert_eq!(
        g.restart(&token, true, f.now()).unwrap_err(),
        GuideError::NotReady
    );
    assert!(
        g.poll_health(f.now())
            .unwrap()
            .iter()
            .all(|e| matches!(e, GuideIntent::Redetect { .. }))
    );
}

#[test]
fn queue_full_wrong_call_duplicate_receipt_and_wrong_node_never_mutate_grants() {
    let f = Fixture::new();
    let mut g = f.loaded();
    let (binding, call) = agent(g.poll_health(f.now()).unwrap());
    let mut raw = f.raw();
    raw["result"]["installer"]["node"] =
        json!("3333333333333333333333333333333333333333333333333333333333333333");
    let result = Ok(DecodedReply::Status(
        parse_status(&serde_json::to_vec(&raw).unwrap(), AgentPlatform::Macos).unwrap(),
    ));
    assert_eq!(
        g.reply(
            &binding,
            AgentReply {
                id: call.id,
                observed_at_ms: f.now(),
                source: ObservationSource::Live,
                result
            },
            f.now()
        )
        .unwrap_err(),
        GuideError::Invalid
    );
    let mut reply = AgentReply {
        id: call.id + 1,
        observed_at_ms: f.now(),
        source: ObservationSource::Live,
        result: Err(CallFailure::QueueFull),
    };
    assert_eq!(
        g.reply(&binding, reply.clone(), f.now()).unwrap_err(),
        GuideError::Stale
    );
    reply.id = call.id;
    let effects = g.reply(&binding, reply.clone(), f.now()).unwrap();
    assert!(
        effects
            .iter()
            .all(|e| !matches!(e, GuideIntent::Agent { .. }))
    );
    assert_eq!(
        g.reply(&binding, reply, f.now()).unwrap_err(),
        GuideError::Stale
    );
    assert!(!g.view(f.now()).audio_prerequisite);
}

#[test]
fn bindings_pin_target_identity_even_when_instances_operations_and_views_match() {
    let f = Fixture::new();
    let other = Fixture::new();
    let mut g = f.loaded();
    let mut other_guide = other.loaded();
    let (binding, call) = agent(g.poll_health(f.now()).unwrap());
    let (foreign, _) = agent(other_guide.poll_health(other.now()).unwrap());
    assert_ne!(binding, foreign);
    assert_eq!(
        g.reply(
            &foreign,
            AgentReply {
                id: call.id,
                observed_at_ms: f.now(),
                source: ObservationSource::Live,
                result: Err(CallFailure::Unavailable)
            },
            f.now()
        )
        .unwrap_err(),
        GuideError::Stale
    );
    assert!(!format!("{binding:?}").contains("cp-guide"));
}

#[test]
fn operation_allocator_is_seeded_and_handed_back_for_sequential_integration() {
    let f = Fixture::new();
    let mut g = PermissionGuide::new(f.admission(), LOCAL.parse().unwrap(), f.now(), 60).unwrap();
    let effects = g.poll_health(f.now()).unwrap();
    assert_eq!(agent(effects.clone()).1.id, 60);
    f.answer(
        &mut g,
        effects,
        Ok(DecodedReply::Status(
            parse_status(&serde_json::to_vec(&f.raw()).unwrap(), AgentPlatform::Macos).unwrap(),
        )),
    );
    let token = g.view(f.now()).token;
    let effects = g.restart(&token, true, f.now()).unwrap();
    assert_eq!(agent(effects).1.id, 61);
    assert_eq!(g.last_operation_id(), 61);
}

#[test]
fn admitted_runtime_and_executable_aliases_match_normalized_native_identity() {
    let f = Fixture::new();
    let path = f.io.target().runtime_dir().join("bootstrap.json");
    let mut bootstrap: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    bootstrap["runtime_dir"] = json!(f.io.target().runtime_dir().to_string_lossy().replacen(
        "/private/tmp",
        "/tmp",
        1
    ));
    bytes(&path, &serde_json::to_vec(&bootstrap).unwrap(), 0o600);
    let mut g = f.guide();
    let mut raw = f.raw();
    for key in ["runtime_dir", "exe"] {
        raw["result"]["installer"]["instance"][key] = json!(
            raw["result"]["installer"]["instance"][key]
                .as_str()
                .unwrap()
                .replacen("/private/tmp", "/tmp", 1)
        );
    }
    f.poll(&mut g, raw);
    assert!(g.view(f.now()).audio_prerequisite);
}

#[test]
fn migration_during_pending_status_retires_every_binding_and_stays_latched() {
    let f = Fixture::new();
    let other = Fixture::new();
    let (mut g, raw) = f.completed();
    let (binding, call) = agent(g.poll_health(f.now()).unwrap());
    let pane = g.open_pane(&g.view(f.now()).token, f.now()).unwrap();
    let GuideIntent::OpenPane {
        binding: pane_binding,
        ..
    } = &pane[0]
    else {
        panic!("pane")
    };
    let old = g.view(f.now()).token;
    other.clock.0.store(f.now(), Ordering::Relaxed);
    let effects = g.redetected(other.admission(), f.now()).unwrap();
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, GuideIntent::RetireActivity { .. }))
    );
    assert_ne!(g.view(f.now()).token, old);
    assert_eq!(
        g.pane_completed(pane_binding, Ok(()), f.now()).unwrap_err(),
        GuideError::Stale
    );
    assert_eq!(
        g.reply(
            &binding,
            AgentReply {
                id: call.id,
                observed_at_ms: f.now(),
                source: ObservationSource::Live,
                result: Ok(DecodedReply::Status(
                    parse_status(&serde_json::to_vec(&raw).unwrap(), AgentPlatform::Macos).unwrap()
                ))
            },
            f.now()
        )
        .unwrap_err(),
        GuideError::Stale
    );
    assert_eq!(
        g.redetected(f.admission(), f.now()).unwrap_err(),
        GuideError::NotReady
    );
    assert_eq!(g.view(f.now()).reason, Some(GuideReason::MigrationPending));
    assert!(!g.view(f.now()).audio_prerequisite);
    assert!(!g.view(f.now()).hiding_next_enabled);
    assert_eq!(g.view(f.now()).parking, ParkingVerification::Pending);
    assert!(
        g.poll_health(f.now())
            .unwrap()
            .iter()
            .all(|e| matches!(e, GuideIntent::Redetect { .. }))
    );
}

#[test]
fn demo_pending_and_native_admission_failure_settle_status_and_retire_proofs() {
    for failure_source in [
        None,
        Some(ObservationSource::Demo),
        Some(ObservationSource::Live),
    ] {
        let native_failure = failure_source.is_some();
        let source = failure_source.unwrap_or(ObservationSource::Demo);
        let f = Fixture::new();
        let (mut g, mut raw) = f.completed();
        let effects = g.poll_health(f.now()).unwrap();
        let (binding, call) = agent(effects.clone());
        let result = if native_failure {
            Err(CallFailure::Unavailable)
        } else {
            raw["result"]["installer"]["schema_version"] = json!(9);
            let status =
                parse_status(&serde_json::to_vec(&raw).unwrap(), AgentPlatform::Macos).unwrap();
            assert!(matches!(status, StatusAdmission::PendingHealthContract(_)));
            Ok(DecodedReply::Status(status))
        };
        let effects = f.answer_source(&mut g, effects, result.clone(), source);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, GuideIntent::RetireActivity { .. }))
        );
        assert!(!g.view(f.now()).audio_prerequisite);
        assert!(
            g.view(f.now())
                .rows
                .iter()
                .all(|r| r.state == PermissionState::Unknown)
        );
        assert!(!g.view(f.now()).hiding_next_enabled);
        assert_eq!(g.view(f.now()).parking, ParkingVerification::Pending);
        assert_eq!(
            g.reply(
                &binding,
                AgentReply {
                    id: call.id,
                    observed_at_ms: f.now(),
                    source,
                    result
                },
                f.now()
            )
            .unwrap_err(),
            GuideError::Stale
        );
        if native_failure {
            assert!(
                effects
                    .iter()
                    .any(|e| matches!(e, GuideIntent::Redetect { .. }))
            );
            assert!(
                g.poll_health(f.now())
                    .unwrap()
                    .iter()
                    .all(|e| matches!(e, GuideIntent::Redetect { .. }))
            );
            f.advance();
            g.redetected(f.admission(), f.now()).unwrap();
        }
        raw["result"]["installer"]["schema_version"] = json!(1);
        f.poll(&mut g, raw);
        assert!(g.view(f.now()).audio_prerequisite);
        assert!(!g.view(f.now()).hiding_next_enabled);
    }
}

#[test]
fn demo_pending_after_settings_restart_settles_frozen_transition_without_live_evidence() {
    let f = Fixture::new();
    let mut g = f.updated(HidingChoice::Hide);
    let effects = g.restart(&g.view(f.now()).token, true, f.now()).unwrap();
    f.answer(&mut g, effects, Ok(DecodedReply::Acknowledged));
    f.advance();
    f.bootstrap(2, "ready");
    g.redetected(f.admission(), f.now()).unwrap();
    let mut raw = f.raw();
    raw["result"]["installer"]
        .as_object_mut()
        .unwrap()
        .remove("permissions");
    let effects = f.poll(&mut g, raw);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, GuideIntent::Redetect { .. }))
    );
    assert!(!g.view(f.now()).audio_prerequisite);
    assert_eq!(
        g.settings_state(),
        &settings_transition::SettingsTransitionState::WaitingNewInstance
    );
    let mut raw = f.raw();
    raw["result"]["installer"]["config_revision"] = json!(NEW);
    f.poll(&mut g, raw);
    assert_eq!(
        g.settings_state(),
        &settings_transition::SettingsTransitionState::Complete
    );
}

#[test]
fn completed_d7_next_expires_and_never_revives_from_fresh_health_alone() {
    let f = Fixture::new();
    let (mut g, raw) = f.completed();
    assert!(g.view(f.now()).hiding_next_enabled);
    f.clock.0.fetch_add(5001, Ordering::Relaxed);
    assert_eq!(g.view(f.now()).parking, ParkingVerification::Pending);
    assert!(!g.view(f.now()).hiding_next_enabled);
    g.tick(f.now()).unwrap();
    f.poll(&mut g, raw);
    assert!(g.view(f.now()).audio_prerequisite);
    assert!(!g.view(f.now()).hiding_next_enabled);
}

#[test]
fn completed_d7_next_retires_on_status_failure_or_failed_recovery() {
    for failed_recovery in [false, true] {
        let f = Fixture::new();
        let (mut g, mut raw) = f.completed();
        if failed_recovery {
            raw["result"]["installer"]["startup_recovery"] = json!("failed");
            f.poll(&mut g, raw.clone());
        } else {
            let effects = g.poll_health(f.now()).unwrap();
            f.answer(&mut g, effects, Err(CallFailure::TimeoutOutcomeUnknown));
        }
        assert!(!g.view(f.now()).hiding_next_enabled);
        assert_eq!(g.view(f.now()).parking, ParkingVerification::Pending);
        if !failed_recovery {
            f.advance();
            g.redetected(f.admission(), f.now()).unwrap();
        }
        raw["result"]["installer"]["startup_recovery"] = json!("restored");
        f.poll(&mut g, raw);
        assert!(!g.view(f.now()).hiding_next_enabled);
    }
}

#[test]
fn restart_dispatch_retires_readiness_before_ack_and_requires_new_recovered_instance() {
    for settings in [false, true] {
        let f = Fixture::new();
        let mut g = if settings {
            f.updated(HidingChoice::Hide)
        } else {
            f.loaded()
        };
        assert!(g.view(f.now()).audio_prerequisite);
        let effects = g.restart(&g.view(f.now()).token, true, f.now()).unwrap();
        assert!(!g.view(f.now()).audio_prerequisite);
        assert!(
            g.view(f.now())
                .rows
                .iter()
                .all(|r| r.state == PermissionState::Unknown)
        );
        assert_eq!(g.view(f.now()).reason, Some(GuideReason::StartupWaiting));
        f.advance();
        g.redetected(f.admission(), f.now()).unwrap();
        f.answer(&mut g, effects, Ok(DecodedReply::Acknowledged));
        assert!(
            g.poll_health(f.now())
                .unwrap()
                .iter()
                .all(|e| matches!(e, GuideIntent::Redetect { .. }))
        );
        assert!(!g.view(f.now()).audio_prerequisite);
        f.advance();
        f.bootstrap(2, "ready");
        g.redetected(f.admission(), f.now()).unwrap();
        let mut raw = f.raw();
        if settings {
            raw["result"]["installer"]["config_revision"] = json!(NEW);
        }
        raw["result"]["installer"]["startup_recovery"] = json!("failed");
        f.poll(&mut g, raw.clone());
        assert!(!g.view(f.now()).audio_prerequisite);
        raw["result"]["installer"]["startup_recovery"] = json!("restored");
        f.poll(&mut g, raw);
        assert!(g.view(f.now()).audio_prerequisite);
    }
}

#[test]
fn definite_restart_refusals_allow_observation_and_fresh_consent() {
    for error in [
        CallFailure::QueueFull,
        CallFailure::InvalidCall(ContractError::InvalidValue),
        CallFailure::Refused(AgentRefusal::NotSupported),
        CallFailure::Unavailable,
    ] {
        for settings in [false, true] {
            let f = Fixture::new();
            let mut g = if settings {
                f.updated(HidingChoice::Hide)
            } else {
                f.loaded()
            };
            let old = g.view(f.now()).token;
            let effects = g.restart(&old, true, f.now()).unwrap();
            let source = if error == CallFailure::Unavailable {
                ObservationSource::Demo
            } else {
                ObservationSource::Live
            };
            f.answer_source(&mut g, effects, Err(error.clone()), source);
            assert!(!g.view(f.now()).audio_prerequisite);
            assert_eq!(
                g.restart(&old, true, f.now()).unwrap_err(),
                GuideError::Stale
            );
            assert!(
                g.poll_health(f.now())
                    .unwrap()
                    .iter()
                    .all(|e| matches!(e, GuideIntent::Redetect { .. }))
            );
            f.advance();
            g.redetected(f.admission(), f.now()).unwrap();
            let effects = f.poll(&mut g, f.raw());
            assert!(!effects.iter().any(|e| matches!(
                e,
                GuideIntent::Agent {
                    call: AgentCall {
                        request: InstallerRequest::Restart,
                        ..
                    },
                    ..
                }
            )));
            assert!(g.view(f.now()).audio_prerequisite);
            let effects = g.restart(&g.view(f.now()).token, true, f.now()).unwrap();
            assert_eq!(agent(effects).1.request, InstallerRequest::Restart);
        }
    }
}

#[test]
fn uncertain_restart_requires_detection_and_query_without_restoring_old_instance_readiness() {
    for error in [
        CallFailure::TimeoutOutcomeUnknown,
        CallFailure::Unavailable,
        CallFailure::InvalidResponse,
    ] {
        let f = Fixture::new();
        let mut g = f.updated(HidingChoice::Hide);
        let effects = g.restart(&g.view(f.now()).token, true, f.now()).unwrap();
        f.answer(&mut g, effects, Err(error));
        assert!(
            g.poll_health(f.now())
                .unwrap()
                .iter()
                .all(|e| matches!(e, GuideIntent::Redetect { .. }))
        );
        f.advance();
        g.redetected(f.admission(), f.now()).unwrap();
        assert_eq!(
            g.restart(&g.view(f.now()).token, true, f.now())
                .unwrap_err(),
            GuideError::Busy
        );
        let effects = g.poll_health(f.now()).unwrap();
        assert_eq!(agent(effects.clone()).1.request, InstallerRequest::Status);
        f.answer(
            &mut g,
            effects,
            Ok(DecodedReply::Status(
                parse_status(&serde_json::to_vec(&f.raw()).unwrap(), AgentPlatform::Macos).unwrap(),
            )),
        );
        assert!(!g.view(f.now()).audio_prerequisite);
        // Query recovery permits a fresh, explicit retry; it does not restore grants or
        // automatically restart, and the new consent retires query evidence again.
        let retry = g.restart(&g.view(f.now()).token, true, f.now()).unwrap();
        assert_eq!(agent(retry.clone()).1.request, InstallerRequest::Restart);
        assert!(!g.view(f.now()).audio_prerequisite);
        f.answer(&mut g, retry, Ok(DecodedReply::Acknowledged));
        f.advance();
        f.bootstrap(2, "ready");
        g.redetected(f.admission(), f.now()).unwrap();
        let mut raw = f.raw();
        raw["result"]["installer"]["config_revision"] = json!(NEW);
        f.poll(&mut g, raw);
        assert!(g.view(f.now()).audio_prerequisite);
    }
}

#[test]
fn completed_d7_ordinary_restart_requires_consent_new_instance_and_recovered_health() {
    let f = Fixture::new();
    let (mut g, mut raw) = f.completed();
    raw["result"]["controlling"] = json!(PEER);
    f.poll(&mut g, raw);
    assert!(g.view(f.now()).hiding_next_enabled);
    assert!(
        g.view(f.now())
            .restart_warning
            .contains("ends active control")
    );
    let token = g.view(f.now()).token;
    assert_eq!(
        g.restart(&token, false, f.now()).unwrap_err(),
        GuideError::NotReady
    );
    let effects = g.restart(&token, true, f.now()).unwrap();
    assert_eq!(agent(effects.clone()).1.request, InstallerRequest::Restart);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, GuideIntent::RetireActivity { .. }))
    );
    assert!(!g.view(f.now()).audio_prerequisite);
    assert!(!g.view(f.now()).hiding_next_enabled);
    assert_eq!(g.view(f.now()).parking, ParkingVerification::Pending);
    assert!(
        g.view(f.now())
            .rows
            .iter()
            .all(|r| r.state == PermissionState::Unknown)
    );
    assert_eq!(
        g.restart(&token, true, f.now()).unwrap_err(),
        GuideError::Stale
    );
    f.answer(&mut g, effects, Ok(DecodedReply::Acknowledged));
    f.advance();
    g.redetected(f.admission(), f.now()).unwrap();
    assert!(
        g.poll_health(f.now())
            .unwrap()
            .iter()
            .all(|e| matches!(e, GuideIntent::Redetect { .. }))
    );
    assert!(!g.view(f.now()).audio_prerequisite);
    f.advance();
    f.bootstrap(3, "ready");
    g.redetected(f.admission(), f.now()).unwrap();
    let mut raw = f.raw();
    raw["result"]["installer"]["config_revision"] = json!(NEW);
    raw["result"]["installer"]["startup_recovery"] = json!("failed");
    f.poll(&mut g, raw.clone());
    assert!(!g.view(f.now()).audio_prerequisite);
    raw["result"]["installer"]["startup_recovery"] = json!("restored");
    f.poll(&mut g, raw);
    assert!(g.view(f.now()).audio_prerequisite);
    assert!(!g.view(f.now()).hiding_next_enabled);
    assert_eq!(g.view(f.now()).hiding_choice, None);
    assert_eq!(
        g.settings_state(),
        &settings_transition::SettingsTransitionState::NeedsDetection
    );
}

#[test]
fn counter_regression_ten_nine_eleven_recovers_only_through_consented_restart() {
    let f = Fixture::new();
    let mut g = f.applied(HidingChoice::Hide);
    let mut raw = f.raw();
    raw["result"]["installer"]["config_revision"] = json!(NEW);
    connected(&mut raw);
    raw["result"]["installer"]["peers"][0]["counters"]["e2_source_started"] = json!(10);
    f.poll(&mut g, raw.clone());
    let token = g.view(f.now()).token;
    g.begin_projection_check(&token, PEER.parse().unwrap(), f.now())
        .unwrap();
    for count in [9, 11] {
        raw["result"]["installer"]["peers"][0]["counters"]["e2_source_started"] = json!(count);
        raw["result"]["installer"]["peers"][0]["last_source_parking"] = json!("twin");
        f.poll(&mut g, raw.clone());
        assert_eq!(
            g.confirm_parking(&token, ParkingObservation::Hidden, f.now())
                .unwrap_err(),
            GuideError::NotReady
        );
        assert_eq!(
            g.begin_projection_check(&g.view(f.now()).token, PEER.parse().unwrap(), f.now())
                .unwrap_err(),
            GuideError::NotReady
        );
        assert!(!g.view(f.now()).hiding_next_enabled);
    }
    // Same-instance native re-admission cannot clear counter poisoning.
    f.advance();
    g.redetected(f.admission(), f.now()).unwrap();
    f.poll(&mut g, raw);
    assert_eq!(
        g.begin_projection_check(&g.view(f.now()).token, PEER.parse().unwrap(), f.now())
            .unwrap_err(),
        GuideError::NotReady
    );
    // Recover through the guide's explicit restart, not an injected replacement instance.
    let effects = g.restart(&g.view(f.now()).token, true, f.now()).unwrap();
    assert_eq!(agent(effects.clone()).1.request, InstallerRequest::Restart);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, GuideIntent::RetireActivity { .. }))
    );
    assert!(!g.view(f.now()).audio_prerequisite);
    f.answer(&mut g, effects, Ok(DecodedReply::Acknowledged));
    f.advance();
    g.redetected(f.admission(), f.now()).unwrap();
    assert!(
        g.poll_health(f.now())
            .unwrap()
            .iter()
            .all(|e| matches!(e, GuideIntent::Redetect { .. }))
    );
    assert!(!g.view(f.now()).audio_prerequisite);
    f.advance();
    f.bootstrap(3, "ready");
    g.redetected(f.admission(), f.now()).unwrap();
    let mut raw = f.raw();
    raw["result"]["installer"]["config_revision"] = json!(NEW);
    f.poll(&mut g, raw);
    assert!(g.view(f.now()).audio_prerequisite);
    // Earlier D7 activity evidence is retired; re-consent and verify it on the new instance.
    g.choose_hiding(&g.view(f.now()).token, HidingChoice::Hide, f.now())
        .unwrap();
    let effects = g.commit_hiding(&g.view(f.now()).token, f.now()).unwrap();
    f.answer(
        &mut g,
        effects,
        Ok(DecodedReply::SettingsUpdated(SettingsUpdated {
            revision: NEW.into(),
            restart_required: true,
        })),
    );
    let effects = g.restart(&g.view(f.now()).token, true, f.now()).unwrap();
    f.answer(&mut g, effects, Ok(DecodedReply::Acknowledged));
    f.advance();
    f.bootstrap(4, "ready");
    g.redetected(f.admission(), f.now()).unwrap();
    let mut raw = f.raw();
    raw["result"]["installer"]["config_revision"] = json!(NEW);
    connected(&mut raw);
    f.poll(&mut g, raw.clone());
    let token = g.view(f.now()).token;
    g.begin_projection_check(&token, PEER.parse().unwrap(), f.now())
        .unwrap();
    raw["result"]["installer"]["peers"][0]["counters"]["e2_source_started"] = json!(1);
    raw["result"]["installer"]["peers"][0]["last_source_parking"] = json!("twin");
    f.poll(&mut g, raw);
    assert_eq!(
        g.confirm_parking(&token, ParkingObservation::Hidden, f.now())
            .unwrap(),
        ParkingVerification::Twin
    );
}

#[test]
fn new_admitted_instance_clears_poison_and_retains_returned_settings_verification() {
    let f = Fixture::new();
    let mut g = f.updated(HidingChoice::Hide);
    let mut raw = f.raw();
    connected(&mut raw);
    for count in [10, 9] {
        raw["result"]["installer"]["peers"][0]["counters"]["e2_source_started"] = json!(count);
        f.poll(&mut g, raw.clone());
    }
    let effects = g.restart(&g.view(f.now()).token, true, f.now()).unwrap();
    f.answer(&mut g, effects, Ok(DecodedReply::Acknowledged));
    f.advance();
    f.bootstrap(2, "ready");
    g.redetected(f.admission(), f.now()).unwrap();
    let mut raw = f.raw();
    connected(&mut raw);
    raw["result"]["installer"]["config_revision"] = json!(NEW);
    f.poll(&mut g, raw.clone());
    assert_eq!(
        g.settings_state(),
        &settings_transition::SettingsTransitionState::Complete
    );
    let token = g.view(f.now()).token;
    g.begin_projection_check(&token, PEER.parse().unwrap(), f.now())
        .unwrap();
    raw["result"]["installer"]["peers"][0]["counters"]["e2_source_started"] = json!(1);
    raw["result"]["installer"]["peers"][0]["last_source_parking"] = json!("twin");
    f.poll(&mut g, raw);
    assert_eq!(
        g.confirm_parking(&token, ParkingObservation::Hidden, f.now())
            .unwrap(),
        ParkingVerification::Twin
    );
}
