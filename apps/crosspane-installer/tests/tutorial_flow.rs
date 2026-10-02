#![allow(clippy::unwrap_used)]
use crosspane_installer::{agent_contract::*, tutorial_flow::*};
use crosspane_installer_core::*;
use crosspane_types::id::{NodeId, WindowId};
use serde_json::{Value, json};

// Literal producer shape; Live is emulated only by this in-memory test adapter.
const STATUS: &[u8] = br#"{"ok":true,"result":{"controlling":null,"controlled_by":null,"projections":[],
"displays":[],"peers":[],"layout":[],"installer":{"schema_version":1,
"build":{"version":"0.0.0","features":[]},"instance":{"id":99,"pid":1,"uid":1000,"exe":"fixture","runtime_dir":"fixture","started_unix_ms":1},
"config_revision":"1111111111111111","node":"1111111111111111111111111111111111111111111111111111111111111111",
"recovery_pending":0,"startup_recovery":"restored","gate":{"open":true,"session":"unlocked","active":true,"armed":true,"panic":false},
"epochs":{"gate":1,"grants":1,"layout":1,"backends":1},"keystore":"os_store","permissions":[],
"backends":[{"name":"capture","state":"ready","reason":null},{"name":"keys","state":"ready","reason":null},{"name":"pointer","state":"ready","reason":null},
{"name":"overlay","state":"ready","reason":null},{"name":"hotkeys","state":"ready","reason":null},{"name":"keystore","state":"ready","reason":null},
{"name":"windows","state":"ready","reason":null},{"name":"parking","state":"ready","reason":null},{"name":"frames","state":"ready","reason":null},
{"name":"tray","state":"ready","reason":null},{"name":"links","state":"ready","reason":null},{"name":"gpu","state":"ready","reason":null},
{"name":"home","state":"ready","reason":null},{"name":"audio","state":"ready","reason":null},{"name":"discovery","state":"ready","reason":null}],
"discovery":{"enabled":true,"running":true,"candidates":0,"error":null},"tray":{"created":true},
"audio":{"enabled":true,"active_peers":[],"frames_sent":0,"frames_played":0},"settings_opened":0,
"peers":[{"node":"2222222222222222222222222222222222222222222222222222222222222222","name":"sensitive peer","connected":true,"link_generation":2,
"features":[],"grants_given":["browse","input","present","share","speaker"],"last_source_parking":null,
"counters":{"e1_controller_started":0,"e1_controller_ended":0,"e1_target_started":0,"e1_target_ended":0,"e1_injections_ok":0,"e1_hud_shows":0,
"e1_chord_releases":0,"e1_command_releases":0,"e2_source_started":0,"e2_source_returned":0,"e2_dest_started":0,"e2_dest_returned":0,
"e2_frames_presented":0,"e2_returns_failed":0}}]}}}"#;
fn local() -> NodeId {
    NodeId([0x11; 32])
}
fn peer() -> NodeId {
    NodeId([0x22; 32])
}
fn value() -> Value {
    serde_json::from_slice(STATUS).unwrap()
}
fn health(v: &Value, platform: AgentPlatform) -> Box<HealthSnapshot> {
    match parse_status(&serde_json::to_vec(v).unwrap(), platform).unwrap() {
        StatusAdmission::Supported(h) => h,
        p => panic!("unexpected admission {p:?}"),
    }
}
fn context() -> TutorialContext {
    TutorialContext {
        machine_label: "own fixture A".into(),
        platform: AgentPlatform::Linux,
        source_policy: TutorialSourcePolicy::Native,
        speakers: Some(TutorialSpeakers {
            peer: peer(),
            device_key: "validated virtual output".into(),
        }),
    }
}
fn fixture_role(r: TutorialRole) -> bool {
    matches!(
        r,
        TutorialRole::E1Target
            | TutorialRole::E2SourcePush
            | TutorialRole::E2SourcePull
            | TutorialRole::AudioSender
    )
}
struct Harness {
    tutorial: Tutorial,
    core: Flow,
    port: AgentQueue,
    calls: Vec<AgentCall>,
    fixtures: Vec<(u64, TutorialFixtureAction)>,
    observed: Vec<TutorialEffect>,
    value: Value,
    now: u64,
    sequence: u64,
    attempt: TutorialAttempt,
    platform: AgentPlatform,
    binding: Option<EvidenceBinding>,
    ids: std::collections::BTreeSet<u64>,
    core_errors: Vec<FlowError>,
    retired: std::collections::BTreeSet<u64>,
}
impl Harness {
    fn new(role: TutorialRole) -> Self {
        Self::with_context(role, context())
    }
    fn with_context(role: TutorialRole, context: TutorialContext) -> Self {
        let mut specs: Vec<_> = roles()
            .iter()
            .enumerate()
            .map(|(i, r)| StepSpec {
                id: StepId(i as u16 + 1),
                prerequisites: vec![],
                required_for_installed: false,
                required_for_ready: false,
                requires_fresh_observation: false,
                requires_activity: true,
                requires_human: true,
                requires_fixture: fixture_role(*r),
            })
            .collect();
        specs.push(StepSpec {
            id: StepId(99),
            prerequisites: vec![],
            required_for_installed: true,
            required_for_ready: true,
            requires_fresh_observation: true,
            requires_activity: false,
            requires_human: false,
            requires_fixture: false,
        });
        let mut core = Flow::new(specs).unwrap();
        let step = StepId(roles().iter().position(|r| *r == role).unwrap() as u16 + 1);
        let detect = core.reduce(FlowEvent::Begin { step }, 1).unwrap().remove(0);
        let verify = core
            .reduce(
                FlowEvent::Detected {
                    step,
                    operation: detect.operation,
                    needs_action: false,
                },
                1,
            )
            .unwrap()
            .remove(0);
        let attempt = TutorialAttempt {
            step,
            operation: verify.operation,
            attempt: AttemptId(1),
            local: local(),
            peer: (role != TutorialRole::Menu).then_some(peer()),
            role,
        };
        let platform = context.platform;
        let mut tutorial = Tutorial::new();
        let effects = tutorial
            .begin(attempt.clone(), context, verify, 17, 1)
            .unwrap();
        let mut h = Self {
            tutorial,
            core,
            port: AgentQueue::default(),
            calls: vec![],
            fixtures: vec![],
            observed: vec![],
            value: value(),
            now: 1,
            sequence: 0,
            attempt,
            platform,
            binding: None,
            ids: Default::default(),
            core_errors: vec![],
            retired: Default::default(),
        };
        if platform == AgentPlatform::Macos {
            h.mac(true);
        }
        h.consume(effects);
        h
    }
    fn consume(&mut self, effects: Vec<TutorialEffect>) {
        self.consume_batch(effects, false);
        self.calls.extend(self.port.take_calls());
    }
    fn consume_batch(&mut self, effects: Vec<TutorialEffect>, recovery: bool) {
        if !effects.is_empty() {
            assert!(matches!(
                effects[0].kind,
                TutorialEffectKind::Core(FlowEvent::Observe { .. })
            ));
        }
        for effect in effects {
            assert_eq!(effect.binding.attempt, self.attempt);
            assert_eq!(effect.binding.view_revision, 17);
            if self.binding.is_some() {
                assert_eq!(effect.binding.evidence, self.binding);
            }
            if let Some(binding) = &effect.binding.evidence {
                assert_eq!(binding.local_node, self.attempt.local);
                assert_eq!(binding.peer, self.attempt.peer);
                if let Some(admitted) = &self.binding {
                    assert_eq!(binding, admitted, "immutable full effect binding");
                } else {
                    self.binding = Some(binding.clone());
                }
            }
            self.observed.push(effect.clone());
            match effect.kind {
                TutorialEffectKind::Core(event) => {
                    if matches!(event, FlowEvent::Invalidate { step } if step == self.attempt.step)
                    {
                        self.retired.insert(self.attempt.attempt.0);
                    }
                    if matches!(event, FlowEvent::Verified { .. }) {
                        assert!(
                            !self.retired.contains(&self.attempt.attempt.0),
                            "no Verified after retirement"
                        );
                    }
                    if let FlowEvent::Observe { samples } = &event {
                        assert_eq!(samples, self.tutorial.current_samples());
                        for sample in samples {
                            assert_eq!(sample.binding.local_node, local());
                            assert_eq!(
                                sample.values.len(),
                                if sample.binding.peer.is_some() {
                                    16 + usize::from(
                                        sample.values.contains_key(&CounterId(
                                            Metric::FramesPresented as u16,
                                        )),
                                    )
                                } else {
                                    3
                                }
                            );
                        }
                    }
                    if recovery {
                        assert!(
                            !matches!(event, FlowEvent::Verified { .. }),
                            "retired evidence cannot verify"
                        );
                    }
                    if let Err(error) = self.core.reduce(event, self.now) {
                        self.core_errors.push(error);
                        if recovery {
                            continue;
                        }
                        let cleanup = self
                            .tutorial
                            .reduce(TutorialEvent::CoreRejected(error), self.now)
                            .unwrap();
                        self.consume_batch(cleanup, true);
                        break;
                    }
                }
                TutorialEffectKind::Agent(call) => {
                    if recovery {
                        assert!(matches!(
                            call.request,
                            InstallerRequest::Status
                                | InstallerRequest::Release
                                | InstallerRequest::Return { .. }
                        ));
                    }
                    assert!(self.ids.insert(call.id));
                    self.tutorial.submitted(call.id).unwrap();
                    self.port.submit(call).unwrap();
                }
                TutorialEffectKind::Fixture { call_id, action } => {
                    if recovery {
                        assert!(matches!(
                            action,
                            TutorialFixtureAction::ObserveWindow { .. }
                                | TutorialFixtureAction::StopTone { .. }
                                | TutorialFixtureAction::Close { .. }
                        ));
                    }
                    assert!(self.ids.insert(call_id));
                    self.tutorial.submitted(call_id).unwrap();
                    self.fixtures.push((call_id, action))
                }
                TutorialEffectKind::WaitForUser
                | TutorialEffectKind::WaitForPeer
                | TutorialEffectKind::WaitForContract
                | TutorialEffectKind::DetectAfterUnknown => {}
            }
        }
    }
    fn event(&mut self, event: TutorialEvent) {
        self.now += 1;
        let effects = self.tutorial.reduce(event, self.now).unwrap();
        self.consume(effects);
    }
    fn call(&mut self, request: &InstallerRequest) -> AgentCall {
        let i = self
            .calls
            .iter()
            .position(|c| &c.request == request)
            .unwrap_or_else(|| panic!("missing {request:?}; {:?}", self.calls));
        self.calls.remove(i)
    }
    fn reply(&mut self, call: AgentCall, result: Result<DecodedReply, CallFailure>) {
        self.now += 1;
        self.port
            .push_reply(AgentReply {
                id: call.id,
                observed_at_ms: self.now,
                source: ObservationSource::Live,
                result,
            })
            .unwrap();
        for reply in self.port.poll() {
            let effects = self
                .tutorial
                .reduce(TutorialEvent::Reply(reply), self.now)
                .unwrap();
            self.consume(effects);
        }
    }
    fn status(&mut self) {
        if !self
            .calls
            .iter()
            .any(|c| c.request == InstallerRequest::Status)
        {
            self.event(TutorialEvent::Tick);
        }
        let call = self.call(&InstallerRequest::Status);
        self.reply(
            call,
            Ok(DecodedReply::Status(StatusAdmission::Supported(health(
                &self.value,
                self.platform,
            )))),
        );
    }
    fn ack(&mut self, request: InstallerRequest) {
        let call = self.call(&request);
        let ack = decode_reply(
            &request,
            br#"{"ok":true,"result":"arbitrary producer prose"}"#,
            self.platform,
        )
        .unwrap();
        self.reply(call, Ok(ack));
    }
    fn fixture_action(&mut self, matches: impl Fn(&TutorialFixtureAction) -> bool) -> u64 {
        let i = self
            .fixtures
            .iter()
            .position(|(_, a)| matches(a))
            .unwrap_or_else(|| panic!("fixture actions {:?}", self.fixtures));
        self.fixtures.remove(i).0
    }
    fn fixture(
        &mut self,
        call_id: Option<u64>,
        result: Result<TutorialFixtureObservation, TutorialFixtureError>,
    ) {
        self.sequence += 1;
        self.event(TutorialEvent::Fixture {
            attempt: self.attempt.attempt,
            call_id,
            sequence: self.sequence,
            observed_at_ms: self.now + 1,
            result,
        });
    }
    fn open(&mut self) {
        let id = self.fixture_action(|a| matches!(a, TutorialFixtureAction::Open { .. }));
        self.fixture(
            Some(id),
            Ok(TutorialFixtureObservation::Opened {
                fixture: 10,
                pid: 123,
                window: WindowId(100),
                label: "own fixture A".into(),
            }),
        );
    }
    fn confirm(&mut self, c: HumanConfirmation) {
        self.event(TutorialEvent::User {
            attempt: self.attempt.attempt,
            view_revision: 17,
            action: TutorialUserAction::Confirm(c),
        });
    }
    fn counter(&mut self, name: &str, n: u64) {
        self.value["result"]["installer"]["peers"][0]["counters"][name] = json!(n);
    }
    fn projection(&mut self, source: NodeId, on: bool) {
        self.value["result"]["projections"] = if on {
            json!([{ "source":source.short(),"projection":55,"text":"sensitive title", "received":{"frames":999999,"bytes":999999} }])
        } else {
            json!([])
        };
    }
    fn home(&mut self) {
        let id = self.fixture_action(|a| matches!(a, TutorialFixtureAction::ObserveWindow { .. }));
        self.fixture(
            Some(id),
            Ok(snapshot(
                None,
                0,
                TutorialWindowFacts::Present {
                    visible_on_user_workspace: Some(true),
                    on_initial_display: Some(true),
                },
            )),
        );
    }
    fn close(&mut self) {
        let id = self.fixture_action(|a| matches!(a, TutorialFixtureAction::Close { .. }));
        self.fixture(
            Some(id),
            Ok(TutorialFixtureObservation::Closed { fixture: 10 }),
        );
    }
    fn verified(&self) {
        assert_eq!(
            self.tutorial.state(),
            TutorialState::Verified,
            "{:?}",
            self.tutorial.detail()
        );
        assert_eq!(
            self.core
                .summary(self.now, self.tutorial.current_samples())
                .steps
                .iter()
                .find(|s| s.id == self.attempt.step)
                .unwrap()
                .state,
            StepState::Satisfied
        );
    }
    fn mac(&mut self, microphone: bool) {
        let mut p = json!([{"name":"screen_recording","state":"granted"},{"name":"accessibility","state":"granted"},{"name":"input_monitoring","state":"granted"}]);
        if microphone {
            p.as_array_mut()
                .unwrap()
                .push(json!({"name":"microphone","state":"granted"}));
        }
        self.value["result"]["installer"]["permissions"] = p;
    }
    fn next(&mut self, role: TutorialRole, peer: Option<NodeId>) {
        self.binding = None;
        let step = StepId(roles().iter().position(|r| *r == role).unwrap() as u16 + 1);
        let detect = self
            .core
            .reduce(FlowEvent::Begin { step }, self.now)
            .unwrap()
            .remove(0);
        let verify = self
            .core
            .reduce(
                FlowEvent::Detected {
                    step,
                    operation: detect.operation,
                    needs_action: false,
                },
                self.now,
            )
            .unwrap()
            .remove(0);
        self.attempt = TutorialAttempt {
            step,
            operation: verify.operation,
            attempt: AttemptId(self.attempt.attempt.0 + 1),
            local: local(),
            peer,
            role,
        };
        let effects = self
            .tutorial
            .begin(self.attempt.clone(), context(), verify, 17, self.now)
            .unwrap();
        self.consume(effects);
    }
}
fn snapshot(
    phase: Option<u64>,
    clicks: u64,
    window_facts: TutorialWindowFacts,
) -> TutorialFixtureObservation {
    TutorialFixtureObservation::Snapshot {
        fixture: 10,
        window: WindowId(100),
        phase,
        pattern_ticks: 10,
        target_clicks: clicks,
        window_facts,
        tone: TutorialToneState::Stopped,
    }
}
fn roles() -> [TutorialRole; 9] {
    use TutorialRole::*;
    [
        E1Controller,
        E1Target,
        E2SourcePush,
        E2DestinationPush,
        E2SourcePull,
        E2DestinationPull,
        AudioSender,
        AudioReceiver,
        Menu,
    ]
}
fn controller_end(h: &mut Harness, chord: bool) {
    h.counter("e1_controller_started", 1);
    h.counter("e1_controller_ended", 1);
    h.counter(
        if chord {
            "e1_chord_releases"
        } else {
            "e1_command_releases"
        },
        1,
    );
    h.status();
}
#[test]
fn e1_controller_isolated_chord_passes_without_ledger_claim() {
    let mut h = Harness::new(TutorialRole::E1Controller);
    h.status();
    controller_end(&mut h, true);
    h.confirm(HumanConfirmation::RemotePracticeAndHud);
    h.verified();
    assert!(!h.tutorial.detail().held_ledger_settlement_observed);
}
#[test]
fn controller_command_release_extra_session_and_no_session_do_not_pass() {
    for (start, end, chord, command) in [(1, 1, 0, 1), (0, 0, 0, 0), (2, 2, 2, 0)] {
        let mut h = Harness::new(TutorialRole::E1Controller);
        h.status();
        h.counter("e1_controller_started", start);
        h.counter("e1_controller_ended", end);
        h.counter("e1_chord_releases", chord);
        h.counter("e1_command_releases", command);
        h.status();
        if h.tutorial.state() != TutorialState::Failed {
            h.confirm(HumanConfirmation::RemotePracticeAndHud);
        }
        assert_ne!(h.tutorial.state(), TutorialState::Verified);
    }
}
#[test]
fn target_own_armed_click_and_local_hud_end_do_not_require_remote_chord() {
    let mut h = Harness::new(TutorialRole::E1Target);
    h.status();
    h.open();
    let arm = h.fixture_action(|a| matches!(a, TutorialFixtureAction::ArmTarget { phase: 1, .. }));
    h.fixture(
        Some(arm),
        Ok(TutorialFixtureObservation::TargetArmed {
            fixture: 10,
            phase: 1,
        }),
    );
    h.fixture(None, Ok(snapshot(Some(1), 1, TutorialWindowFacts::Unknown)));
    for name in [
        "e1_target_started",
        "e1_target_ended",
        "e1_injections_ok",
        "e1_hud_shows",
    ] {
        h.counter(name, 1);
    }
    h.status();
    h.confirm(HumanConfirmation::ControllerCrossingAndRelease);
    h.close();
    h.verified();
    assert_eq!(
        h.value["result"]["installer"]["peers"][0]["counters"]["e1_chord_releases"],
        0
    );
}
#[test]
fn menu_requires_spawn_counter_tray_and_real_human_confirmation() {
    let mut h = Harness::new(TutorialRole::Menu);
    h.status();
    h.confirm(HumanConfirmation::TrayAndSettingsVisible);
    assert_ne!(h.tutorial.state(), TutorialState::Verified);
    h.value["result"]["installer"]["settings_opened"] = json!(1);
    h.status();
    h.verified();
}
fn start_e2(role: TutorialRole) -> Harness {
    let mut h = Harness::new(role);
    h.status();
    authorize_e2(&mut h);
    observe_e2(&mut h);
    h
}
fn authorize_e2(h: &mut Harness) {
    let role = h.attempt.role;
    match role {
        TutorialRole::E2SourcePush => {
            h.open();
            let call = h.call(&InstallerRequest::Windows);
            h.reply(call, Ok(decode_reply(&InstallerRequest::Windows, br#"{"ok":true,"result":[{"id":100,"app":"fixture","title":"sensitive","display":null,"size":[20.5,30.5]}]}"#, h.platform).unwrap()));
            h.ack(InstallerRequest::Project {
                window: WindowId(100),
                peer: peer(),
            });
        }
        TutorialRole::E2SourcePull => {
            h.open();
            h.confirm(HumanConfirmation::SourceMachineAndAttempt);
        }
        TutorialRole::E2DestinationPull => {
            let call = h.call(&InstallerRequest::WindowsFrom { peer: peer() });
            h.reply(call, Ok(decode_reply(&InstallerRequest::WindowsFrom { peer:peer() }, br#"{"ok":true,"result":[{"id":100,"app":"fixture","title":"advisory","size":[20,30]}]}"#, h.platform).unwrap()));
            h.event(TutorialEvent::User {
                attempt: h.attempt.attempt,
                view_revision: 17,
                action: TutorialUserAction::SelectRemoteWindow {
                    window: WindowId(100),
                },
            });
            h.confirm(HumanConfirmation::SourceMachineAndAttempt);
            h.ack(InstallerRequest::Pull {
                peer: peer(),
                window: WindowId(100),
            });
        }
        TutorialRole::E2DestinationPush => h.confirm(HumanConfirmation::SourceMachineAndAttempt),
        _ => unreachable!(),
    }
}
fn observe_e2(h: &mut Harness) {
    let role = h.attempt.role;
    let source = matches!(
        role,
        TutorialRole::E2SourcePush | TutorialRole::E2SourcePull
    );
    h.projection(if source { local() } else { peer() }, true);
    let metric = if source {
        "e2_source_started"
    } else {
        "e2_dest_started"
    };
    let n = h.value["result"]["installer"]["peers"][0]["counters"][metric]
        .as_u64()
        .unwrap();
    h.counter(metric, n + 1);
    if source {
        h.value["result"]["installer"]["recovery_pending"] = json!(1);
        h.value["result"]["installer"]["peers"][0]["last_source_parking"] = json!("twin");
    }
    h.status();
    assert_eq!(
        h.tutorial.state(),
        TutorialState::Running,
        "{:?}",
        h.tutorial.detail()
    );
}
fn end_e2(h: &mut Harness) {
    let source = matches!(
        h.attempt.role,
        TutorialRole::E2SourcePush | TutorialRole::E2SourcePull
    );
    h.confirm(HumanConfirmation::DestinationPatternInteractionAndClose);
    h.ack(InstallerRequest::Return {
        projection: 55,
        source: (!source).then_some(peer()),
    });
    assert_ne!(h.tutorial.state(), TutorialState::Verified);
    h.projection(local(), false);
    let metric = if source {
        "e2_source_returned"
    } else {
        "e2_dest_returned"
    };
    let n = h.value["result"]["installer"]["peers"][0]["counters"][metric]
        .as_u64()
        .unwrap();
    h.counter(metric, n + 1);
    h.value["result"]["installer"]["recovery_pending"] = json!(0);
    if !source {
        let n = h.value["result"]["installer"]["peers"][0]["counters"]["e2_frames_presented"]
            .as_u64()
            .unwrap();
        h.counter("e2_frames_presented", n + 10);
    }
    h.status();
    if source {
        h.home();
        if h.fixtures
            .iter()
            .any(|(_, a)| matches!(a, TutorialFixtureAction::Close { .. }))
        {
            h.close();
        }
    } else {
        h.confirm(HumanConfirmation::SourceRestored);
    }
}
#[test]
fn e2_push_and_pull_both_local_roles_are_distinct_owned_attempts() {
    for role in [
        TutorialRole::E2SourcePush,
        TutorialRole::E2SourcePull,
        TutorialRole::E2DestinationPush,
        TutorialRole::E2DestinationPull,
    ] {
        let mut h = start_e2(role);
        end_e2(&mut h);
        h.verified();
        assert!(!h.observed.iter().any(|e| matches!(
            e.kind,
            TutorialEffectKind::Fixture {
                action: TutorialFixtureAction::ArmTarget { .. },
                ..
            }
        )));
    }
}
#[test]
fn presented_null_remains_pending_despite_populated_decoder_statistics() {
    let mut h = start_e2(TutorialRole::E2DestinationPush);
    h.value["result"]["installer"]["peers"][0]["counters"]["e2_frames_presented"] = Value::Null;
    // Removing a previously available value also invalidates core; neither form is zero.
    h.status();
    assert_ne!(h.tutorial.state(), TutorialState::Verified);
    assert_eq!(
        h.tutorial.current_samples()[1].values.get(&CounterId(12)),
        None
    );
}
fn audio_start(sender: bool) -> Harness {
    let mut h = Harness::new(if sender {
        TutorialRole::AudioSender
    } else {
        TutorialRole::AudioReceiver
    });
    h.status();
    if sender {
        h.open();
        h.event(TutorialEvent::User {
            attempt: AttemptId(1),
            view_revision: 17,
            action: TutorialUserAction::PlayTestSound,
        });
        let id = h.fixture_action(|a| {
            *a == TutorialFixtureAction::PlayTone {
                fixture: 10,
                peer: peer(),
                device_key: "validated virtual output".into(),
            }
        });
        h.fixture(
            Some(id),
            Ok(TutorialFixtureObservation::ToneStarted {
                fixture: 10,
                tone: 33,
            }),
        );
    } else {
        h.confirm(HumanConfirmation::SelectedSourceToneStarted);
    }
    h.value["result"]["installer"]["audio"]["active_peers"] = json!([peer()]);
    h.status();
    h
}
fn audio_end(h: &mut Harness, sender: bool) {
    h.value["result"]["installer"]["audio"][if sender {
        "frames_sent"
    } else {
        "frames_played"
    }] = json!(10);
    h.status();
    if sender {
        let id =
            h.fixture_action(|a| matches!(a, TutorialFixtureAction::StopTone { tone: 33, .. }));
        h.fixture(
            Some(id),
            Ok(TutorialFixtureObservation::ToneStopped {
                fixture: 10,
                tone: 33,
            }),
        );
    }
    h.confirm(if sender {
        HumanConfirmation::FarSpeakerHeard
    } else {
        HumanConfirmation::LocalSpeakerHeard
    });
    h.confirm(HumanConfirmation::ExclusiveAudioInterval);
    if sender {
        h.close();
    }
}
#[test]
fn audio_sender_own_selected_tone_stopped_and_human_attestation_passes() {
    let mut h = audio_start(true);
    audio_end(&mut h, true);
    h.verified();
}
#[test]
fn audio_receiver_global_played_with_local_hearing_and_attestation_passes() {
    let mut h = audio_start(false);
    audio_end(&mut h, false);
    h.verified();
    assert!(h.fixtures.is_empty());
}
#[test]
fn human_only_missing_target_click_wrong_phase_and_routing_do_not_pass() {
    for phase in [None, Some(2)] {
        let mut h = Harness::new(TutorialRole::E1Target);
        h.status();
        h.open();
        let arm = h.fixture_action(|a| matches!(a, TutorialFixtureAction::ArmTarget { .. }));
        h.fixture(
            Some(arm),
            Ok(TutorialFixtureObservation::TargetArmed {
                fixture: 10,
                phase: 1,
            }),
        );
        h.fixture(None, Ok(snapshot(phase, 1, TutorialWindowFacts::Unknown)));
        h.confirm(HumanConfirmation::ControllerCrossingAndRelease);
        for name in [
            "e1_target_started",
            "e1_target_ended",
            "e1_injections_ok",
            "e1_hud_shows",
        ] {
            h.counter(name, 1);
        }
        h.status();
        assert_ne!(h.tutorial.state(), TutorialState::Verified);
        assert!(
            !h.fixtures
                .iter()
                .any(|(_, a)| matches!(a, TutorialFixtureAction::Close { .. }))
        );
    }
    let mut h = Harness::new(TutorialRole::E1Controller);
    h.status();
    h.value["result"]["controlling"] = json!(peer());
    controller_end(&mut h, true);
    h.confirm(HumanConfirmation::RemotePracticeAndHud);
    assert_ne!(h.tutorial.state(), TutorialState::Verified);
}
#[test]
fn all_safety_health_facts_fail_closed_but_active_source_parking_is_allowed() {
    for (path, value) in [
        ("/result/installer/startup_recovery", json!("failed")),
        ("/result/installer/recovery_pending", json!(1)),
        ("/result/installer/keystore", json!("file")),
        ("/result/installer/gate/open", json!(false)),
        ("/result/installer/gate/panic", json!(true)),
        ("/result/installer/gate/session", json!("unknown")),
        ("/result/installer/gate/session", json!("locked")),
        ("/result/installer/gate/active", Value::Null),
        ("/result/installer/gate/active", json!(false)),
    ] {
        let mut h = Harness::new(TutorialRole::E1Controller);
        *h.value.pointer_mut(path).unwrap() = value;
        h.status();
        assert_eq!(h.tutorial.state(), TutorialState::Failed, "{path}");
        assert!(h.fixtures.is_empty());
    }
    let h = start_e2(TutorialRole::E2SourcePush);
    assert_eq!(h.value["result"]["installer"]["recovery_pending"], 1);
    assert_eq!(h.tutorial.state(), TutorialState::Running);
}
#[test]
fn idle_and_terminal_parking_nonzero_cannot_hide_startup_failure_or_complete() {
    let mut h = start_e2(TutorialRole::E2SourcePull);
    h.projection(local(), false);
    h.counter("e2_source_returned", 1);
    h.status();
    assert_eq!(h.tutorial.state(), TutorialState::Failed);
    assert!(!h.tutorial.detail().cleanup_settled);
}
#[test]
fn mac_three_permissions_admit_non_audio_but_audio_needs_enabled_and_microphone() {
    let mut c = context();
    c.platform = AgentPlatform::Macos;
    c.source_policy = TutorialSourcePolicy::MacMirror;
    let mut h = Harness::with_context(TutorialRole::E1Controller, c.clone());
    h.mac(false);
    h.status();
    controller_end(&mut h, true);
    h.confirm(HumanConfirmation::RemotePracticeAndHud);
    h.verified();
    for microphone in [false, true] {
        let mut h = Harness::with_context(TutorialRole::AudioSender, c.clone());
        h.mac(microphone);
        if microphone {
            h.value["result"]["installer"]["audio"]["enabled"] = json!(false);
        }
        h.status();
        assert_eq!(h.tutorial.state(), TutorialState::Failed);
        assert!(h.fixtures.is_empty());
    }
    let mut h = Harness::with_context(TutorialRole::AudioSender, c);
    h.status();
    assert_eq!(h.tutorial.state(), TutorialState::WaitingFixture);
    let mut c = context();
    c.platform = AgentPlatform::Macos;
    c.source_policy = TutorialSourcePolicy::MacMirror;
    let mut h = Harness::with_context(TutorialRole::E1Controller, c);
    h.value["result"]["installer"]["permissions"][3]["state"] = json!("unknown");
    h.status();
    controller_end(&mut h, true);
    h.confirm(HumanConfirmation::RemotePracticeAndHud);
    h.verified();
}
#[test]
fn required_backend_and_selected_local_grant_do_not_borrow_unrelated_peer_facts() {
    let mut h = Harness::new(TutorialRole::E1Target);
    h.value["result"]["installer"]["peers"][0]["grants_given"] = json!([]);
    let mut unrelated = h.value["result"]["installer"]["peers"][0].clone();
    unrelated["node"] = json!(NodeId([0x33; 32]));
    unrelated["grants_given"] = json!(["input"]);
    h.value["result"]["installer"]["peers"]
        .as_array_mut()
        .unwrap()
        .push(unrelated);
    h.status();
    assert_eq!(h.tutorial.state(), TutorialState::Failed);
    for state in ["missing", "blocked", "failed"] {
        let mut h = Harness::new(TutorialRole::E1Controller);
        h.value["result"]["installer"]["backends"][0]["state"] = json!(state);
        h.value["result"]["installer"]["backends"][0]["reason"] = json!("permission");
        h.status();
        assert_eq!(h.tutorial.state(), TutorialState::Failed);
    }
}
#[test]
fn explicit_machine_source_speaker_and_verify_context_reject_invalid_begins() {
    let h = Harness::new(TutorialRole::AudioSender);
    let a = h.attempt.clone();
    let job = JobIntent {
        step: a.step,
        operation: a.operation,
        stage: JobStage::Verify,
    };
    let mut contexts = vec![];
    let mut c = context();
    c.machine_label = "x".repeat(65);
    contexts.push(c);
    let mut c = context();
    c.machine_label = "control\n".into();
    contexts.push(c);
    let mut c = context();
    c.source_policy = TutorialSourcePolicy::MacMirror;
    contexts.push(c);
    let mut c = context();
    c.speakers = None;
    contexts.push(c);
    let mut c = context();
    c.speakers.as_mut().unwrap().peer = local();
    contexts.push(c);
    let mut c = context();
    c.speakers.as_mut().unwrap().device_key = "x".repeat(161);
    contexts.push(c);
    for c in contexts {
        assert!(
            Tutorial::new()
                .begin(a.clone(), c, job.clone(), 17, 1)
                .is_err()
        );
    }
    let mut c = context();
    c.platform = AgentPlatform::Macos;
    assert!(
        Tutorial::new()
            .begin(a.clone(), c, job.clone(), 17, 1)
            .is_err()
    );
    for stage in [JobStage::Detect, JobStage::Plan, JobStage::Apply] {
        let mut bad = job.clone();
        bad.stage = stage;
        assert_eq!(
            Tutorial::new().begin(a.clone(), context(), bad, 17, 1),
            Err(TutorialError::WrongOperation)
        );
    }
    let mut exhausted = a;
    exhausted.attempt = AttemptId(u64::MAX);
    assert_eq!(
        Tutorial::new().begin(exhausted, context(), job, 17, 1),
        Err(TutorialError::IdExhausted)
    );
}
#[test]
fn wrong_view_attempt_operation_and_confirmation_role_do_not_refresh_proof() {
    let mut h = Harness::new(TutorialRole::E1Controller);
    h.status();
    let at = h.tutorial.current_samples()[0].observed_at_ms;
    for event in [
        TutorialEvent::User {
            attempt: AttemptId(2),
            view_revision: 17,
            action: TutorialUserAction::Confirm(HumanConfirmation::RemotePracticeAndHud),
        },
        TutorialEvent::User {
            attempt: AttemptId(1),
            view_revision: 18,
            action: TutorialUserAction::Confirm(HumanConfirmation::RemotePracticeAndHud),
        },
        TutorialEvent::CoreJob(JobIntent {
            step: h.attempt.step,
            operation: OperationId(999),
            stage: JobStage::Verify,
        }),
        TutorialEvent::User {
            attempt: AttemptId(1),
            view_revision: 17,
            action: TutorialUserAction::Confirm(HumanConfirmation::FarSpeakerHeard),
        },
    ] {
        assert!(h.tutorial.reduce(event, h.now + 1).is_err());
    }
    assert_eq!(h.tutorial.current_samples()[0].observed_at_ms, at);
}
#[test]
fn stale_duplicate_future_demo_and_wrong_node_agent_replies_are_rejected() {
    let mut h = Harness::new(TutorialRole::E1Controller);
    h.status();
    h.event(TutorialEvent::Tick);
    let call = h.call(&InstallerRequest::Status);
    let reply = AgentReply {
        id: call.id,
        observed_at_ms: h.now,
        source: ObservationSource::Live,
        result: Ok(DecodedReply::Status(StatusAdmission::Supported(health(
            &h.value, h.platform,
        )))),
    };
    let mut future = reply.clone();
    future.observed_at_ms = h.now + 100;
    assert_eq!(
        h.tutorial.reduce(TutorialEvent::Reply(future), h.now),
        Err(TutorialError::InvalidObservation)
    );
    let mut demo = reply.clone();
    demo.source = ObservationSource::Demo;
    assert_eq!(
        h.tutorial.reduce(TutorialEvent::Reply(demo), h.now),
        Err(TutorialError::InvalidObservation)
    );
    let effects = h
        .tutorial
        .reduce(TutorialEvent::Reply(reply.clone()), h.now)
        .unwrap();
    h.consume(effects);
    assert_eq!(
        h.tutorial.reduce(TutorialEvent::Reply(reply), h.now + 1),
        Err(TutorialError::InvalidObservation)
    );
    let mut h = Harness::new(TutorialRole::E1Controller);
    h.value["result"]["installer"]["node"] = json!(NodeId([0x33; 32]));
    let call = h.call(&InstallerRequest::Status);
    assert_eq!(
        h.tutorial.reduce(
            TutorialEvent::Reply(AgentReply {
                id: call.id,
                observed_at_ms: 1,
                source: ObservationSource::Live,
                result: Ok(DecodedReply::Status(StatusAdmission::Supported(health(
                    &h.value, h.platform
                ))))
            }),
            1
        ),
        Err(TutorialError::InvalidObservation)
    );
}
#[test]
fn missing_contract_start_stays_pending_without_fixture_or_ack_success() {
    let mut h = Harness::new(TutorialRole::E1Controller);
    let call = h.call(&InstallerRequest::Status);
    h.reply(
        call,
        Ok(DecodedReply::Status(
            StatusAdmission::PendingHealthContract(PendingHealthReason::Incomplete),
        )),
    );
    assert_eq!(h.tutorial.state(), TutorialState::PendingContract);
    assert!(h.fixtures.is_empty());
}
#[test]
fn all_required_epoch_link_restart_changes_retire_and_release_before_more_activity() {
    for path in [
        "/result/installer/epochs/gate",
        "/result/installer/epochs/grants",
        "/result/installer/epochs/layout",
        "/result/installer/epochs/backends",
        "/result/installer/instance/id",
        "/result/installer/peers/0/link_generation",
    ] {
        let mut h = Harness::new(TutorialRole::E1Controller);
        h.status();
        let old = h.value.pointer(path).unwrap().as_u64().unwrap();
        *h.value.pointer_mut(path).unwrap() = json!(old + 1);
        h.status();
        assert_eq!(h.tutorial.state(), TutorialState::Failed, "{path}");
        assert!(
            h.calls
                .iter()
                .any(|c| c.request == InstallerRequest::Release)
        );
        assert!(
            h.tutorial
                .reduce(
                    TutorialEvent::User {
                        attempt: AttemptId(1),
                        view_revision: 17,
                        action: TutorialUserAction::Confirm(
                            HumanConfirmation::RemotePracticeAndHud
                        )
                    },
                    h.now + 1
                )
                .is_err()
        );
    }
}
#[test]
fn audio_required_epochs_exclude_layout_and_menu_excludes_grants() {
    let mut h = audio_start(false);
    h.value["result"]["installer"]["epochs"]["layout"] = json!(2);
    audio_end(&mut h, false);
    h.verified();
    let mut h = Harness::new(TutorialRole::Menu);
    h.status();
    h.value["result"]["installer"]["epochs"]["grants"] = json!(2);
    h.value["result"]["installer"]["settings_opened"] = json!(1);
    h.status();
    h.confirm(HumanConfirmation::TrayAndSettingsVisible);
    h.verified();
}
#[test]
fn fixture_open_and_submit_errors_need_no_fabricated_fixture_and_keep_reason() {
    for error in [
        TutorialFixtureError::Unavailable,
        TutorialFixtureError::TimedOut,
        TutorialFixtureError::ChannelClosed,
    ] {
        let mut h = Harness::new(TutorialRole::E1Target);
        h.status();
        let id = h.fixture_action(|a| matches!(a, TutorialFixtureAction::Open { .. }));
        h.fixture(Some(id), Err(error));
        assert_eq!(h.tutorial.state(), TutorialState::Failed);
        assert_eq!(
            h.tutorial.detail().failure,
            Some(TutorialFailure::Fixture(error))
        );
        assert!(
            !h.fixtures
                .iter()
                .any(|(_, a)| matches!(a, TutorialFixtureAction::Close { .. }))
        );
    }
    let mut h = Harness::new(TutorialRole::E1Target);
    h.status();
    let id = h.fixture_action(|a| matches!(a, TutorialFixtureAction::Open { .. }));
    h.event(TutorialEvent::FixtureSubmitFailed {
        attempt: AttemptId(1),
        call_id: id,
        error: TutorialFixtureError::Busy,
    });
    assert_eq!(
        h.tutorial.detail().failure,
        Some(TutorialFailure::Fixture(TutorialFixtureError::Busy))
    );
}
#[test]
fn fixture_ids_sequences_call_ids_and_receipt_times_cannot_be_replayed() {
    let mut h = Harness::new(TutorialRole::E1Target);
    h.status();
    h.open();
    let arm = h.fixture_action(|a| matches!(a, TutorialFixtureAction::ArmTarget { .. }));
    for (attempt, call_id, sequence, at, fixture) in [
        (2, Some(arm), 2, h.now, 10),
        (1, Some(999), 2, h.now, 10),
        (1, Some(arm), 1, h.now, 10),
        (1, Some(arm), 2, h.now + 100, 10),
        (1, Some(arm), 2, h.now, 11),
    ] {
        assert!(
            h.tutorial
                .reduce(
                    TutorialEvent::Fixture {
                        attempt: AttemptId(attempt),
                        call_id,
                        sequence,
                        observed_at_ms: at,
                        result: Ok(TutorialFixtureObservation::TargetArmed { fixture, phase: 1 })
                    },
                    h.now
                )
                .is_err()
        );
    }
    h.fixture(
        Some(arm),
        Ok(TutorialFixtureObservation::TargetArmed {
            fixture: 10,
            phase: 1,
        }),
    );
    assert!(
        h.tutorial
            .reduce(
                TutorialEvent::Fixture {
                    attempt: AttemptId(1),
                    call_id: Some(arm),
                    sequence: 3,
                    observed_at_ms: h.now,
                    result: Ok(TutorialFixtureObservation::TargetArmed {
                        fixture: 10,
                        phase: 1
                    })
                },
                h.now
            )
            .is_err()
    );
}
#[test]
fn owned_cancel_close_requested_and_command_ack_do_not_settle_cleanup() {
    let mut h = start_e2(TutorialRole::E2SourcePush);
    h.event(TutorialEvent::User {
        attempt: AttemptId(1),
        view_revision: 17,
        action: TutorialUserAction::Cancel,
    });
    assert_eq!(h.tutorial.state(), TutorialState::Cancelled);
    h.ack(InstallerRequest::Return {
        projection: 55,
        source: None,
    });
    assert!(!h.tutorial.detail().cleanup_settled);
    assert!(h.fixtures.is_empty());
    h.projection(local(), false);
    h.counter("e2_source_returned", 1);
    h.value["result"]["installer"]["recovery_pending"] = json!(0);
    h.status();
    h.home();
    let close = h.fixture_action(|a| matches!(a, TutorialFixtureAction::Close { fixture: 10 }));
    h.fixture(
        Some(close),
        Ok(TutorialFixtureObservation::CloseRequested { fixture: 10 }),
    );
    assert!(!h.tutorial.detail().cleanup_settled);
    h.fixture(
        Some(close),
        Ok(TutorialFixtureObservation::Closed { fixture: 10 }),
    );
    assert!(h.tutorial.detail().cleanup_settled);
    assert_eq!(h.tutorial.state(), TutorialState::Cancelled);
}
#[test]
fn unrelated_projection_cannot_be_returned_or_owned_by_title() {
    let mut h = Harness::new(TutorialRole::E2DestinationPush);
    h.projection(peer(), true);
    h.status();
    assert_eq!(h.tutorial.state(), TutorialState::Failed);
    assert!(
        !h.calls
            .iter()
            .any(|c| matches!(c.request, InstallerRequest::Return { .. }))
    );
    let mut h = start_e2(TutorialRole::E2DestinationPush);
    h.value["result"]["projections"]
        .as_array_mut()
        .unwrap()
        .push(json!({"source":peer().short(),"projection":56,"text":"same title","received":null}));
    h.status();
    assert_eq!(h.tutorial.state(), TutorialState::Failed);
    assert!(
        h.calls
            .iter()
            .all(|c| !matches!(c.request, InstallerRequest::Return { projection: 56, .. }))
    );
}
#[test]
fn mutation_timeout_detects_before_retry_and_does_not_parse_projection_prose() {
    let mut h = Harness::new(TutorialRole::E2SourcePush);
    h.status();
    h.open();
    let call = h.call(&InstallerRequest::Windows);
    h.reply(
        call,
        Ok(DecodedReply::Windows(vec![LocalWindow {
            id: WindowId(100),
            app: "fixture".into(),
            title: "title".into(),
            display: None,
            size: [1.0, 1.0],
        }])),
    );
    let call = h.call(&InstallerRequest::Project {
        window: WindowId(100),
        peer: peer(),
    });
    h.reply(call, Err(CallFailure::TimeoutOutcomeUnknown));
    assert!(
        h.observed
            .iter()
            .any(|e| e.kind == TutorialEffectKind::DetectAfterUnknown)
    );
    assert!(
        h.calls
            .iter()
            .any(|c| c.request == InstallerRequest::Status)
    );
    assert!(!h.calls.iter().any(|c| matches!(
        c.request,
        InstallerRequest::Project { .. } | InstallerRequest::Return { .. }
    )));
    assert_eq!(h.tutorial.state(), TutorialState::Failed);
}
#[test]
fn audio_missing_attestation_idle_endpoint_opposite_counter_and_extra_peer_do_not_pass() {
    let mut h = audio_start(false);
    h.value["result"]["installer"]["audio"]["frames_played"] = json!(10);
    h.status();
    h.confirm(HumanConfirmation::LocalSpeakerHeard);
    assert_ne!(h.tutorial.state(), TutorialState::Verified);
    for mutation in [0, 1, 2] {
        let mut h = audio_start(false);
        h.value["result"]["installer"]["audio"]["frames_played"] = json!(10);
        match mutation {
            0 => h.value["result"]["installer"]["audio"]["active_peers"] = json!([]),
            1 => h.value["result"]["installer"]["audio"]["frames_sent"] = json!(1),
            _ => {
                h.value["result"]["installer"]["audio"]["active_peers"] =
                    json!([peer(), NodeId([0x33; 32])])
            }
        }
        h.status();
        assert_eq!(h.tutorial.state(), TutorialState::Failed);
    }
}
#[test]
fn audio_proof_endpoint_is_active_before_idle_cleanup_and_late_click_time() {
    let mut h = audio_start(true);
    h.value["result"]["installer"]["audio"]["frames_sent"] = json!(10);
    h.status();
    let endpoint = h.tutorial.current_samples()[1].observed_at_ms;
    let stop = h.fixture_action(|a| matches!(a, TutorialFixtureAction::StopTone { .. }));
    h.fixture(
        Some(stop),
        Ok(TutorialFixtureObservation::ToneStopped {
            fixture: 10,
            tone: 33,
        }),
    );
    h.value["result"]["installer"]["audio"]["active_peers"] = json!([]);
    h.status();
    h.confirm(HumanConfirmation::FarSpeakerHeard);
    h.confirm(HumanConfirmation::ExclusiveAudioInterval);
    h.close();
    h.verified();
    let v = h
        .observed
        .iter()
        .find_map(|e| match &e.kind {
            TutorialEffectKind::Core(FlowEvent::Verified { verification, .. }) => {
                Some(verification)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(v.observed_at_ms, endpoint);
    assert!(v.observed_at_ms < h.now);
    let detail = h.tutorial.detail();
    assert!(
        detail.global_audio_counters
            && detail.sampled_peer_membership
            && detail.human_hearing_required
            && detail.unseen_between_poll_audio_possible
            && detail.backend_changes_within_one_tick_may_be_missed
    );
    // No synthetic membership event exists for a session entirely between polls; the detail
    // reports the accepted limit rather than claiming continuous/native/per-peer detection.
}
#[test]
fn output_lost_stop_unconfirmed_and_cleanup_errors_preserve_failure() {
    for error in [
        TutorialFixtureError::OutputChanged,
        TutorialFixtureError::OutputUnavailable,
        TutorialFixtureError::CleanupFailed,
        TutorialFixtureError::NotOwned,
    ] {
        let mut h = audio_start(true);
        h.fixture(
            None,
            Ok(TutorialFixtureObservation::Lost {
                fixture: 10,
                reason: error,
            }),
        );
        assert_eq!(h.tutorial.state(), TutorialState::Failed);
        assert_eq!(
            h.tutorial.detail().failure,
            Some(TutorialFailure::Fixture(error))
        );
        assert!(!h.tutorial.detail().cleanup_settled);
    }
    let mut h = audio_start(true);
    let mut s = snapshot(None, 0, TutorialWindowFacts::Unknown);
    if let TutorialFixtureObservation::Snapshot { tone, .. } = &mut s {
        *tone = TutorialToneState::StopUnconfirmed { tone: 33 };
    }
    h.fixture(None, Ok(s));
    assert_eq!(h.tutorial.state(), TutorialState::Failed);
}
#[test]
fn local_private_twin_human_mirror_fallback_and_null_are_honest_results() {
    for parking in [json!("twin"), json!("mirror"), Value::Null] {
        let mut h = start_e2(TutorialRole::E2SourcePull);
        // Change the explicit context before begin, not during a practice. Re-create with Mac.
        let mut c = context();
        c.platform = AgentPlatform::Macos;
        c.source_policy = TutorialSourcePolicy::MacPrivateDisplay;
        let mut mac = Harness::with_context(TutorialRole::E2SourcePull, c);
        mac.status();
        mac.open();
        mac.confirm(HumanConfirmation::SourceMachineAndAttempt);
        mac.projection(local(), true);
        mac.counter("e2_source_started", 1);
        mac.value["result"]["installer"]["peers"][0]["last_source_parking"] = parking.clone();
        mac.status();
        mac.confirm(HumanConfirmation::PrivateDisplayObserved);
        end_e2(&mut mac);
        if parking == json!("twin") {
            mac.verified();
        } else {
            assert_eq!(mac.tutorial.state(), TutorialState::PendingContract);
            assert_eq!(
                mac.tutorial.detail().parking,
                if parking.is_null() {
                    ParkingResult::Unknown
                } else {
                    ParkingResult::MirrorFallback
                }
            );
        }
        // Linux source's current kind cannot be borrowed as remote parking proof.
        h.value["result"]["installer"]["peers"][0]["last_source_parking"] = Value::Null;
        assert_eq!(h.tutorial.detail().parking, ParkingResult::Native);
    }
}
#[test]
fn source_policy_mirror_is_a_complete_fallback_without_private_display_pass() {
    let mut c = context();
    c.platform = AgentPlatform::Macos;
    c.source_policy = TutorialSourcePolicy::MacMirror;
    let mut h = Harness::with_context(TutorialRole::E2SourcePull, c);
    h.status();
    h.open();
    h.confirm(HumanConfirmation::SourceMachineAndAttempt);
    h.projection(local(), true);
    h.counter("e2_source_started", 1);
    h.value["result"]["installer"]["peers"][0]["last_source_parking"] = json!("mirror");
    h.status();
    end_e2(&mut h);
    h.verified();
    assert_eq!(h.tutorial.detail().parking, ParkingResult::MirrorFallback);
    assert!(!h.tutorial.detail().private_display_verified);
}
#[test]
fn null_presented_from_first_sample_waits_for_producer_contract() {
    let mut h = Harness::new(TutorialRole::E2DestinationPull);
    h.value["result"]["installer"]["peers"][0]["counters"]["e2_frames_presented"] = Value::Null;
    h.status();
    assert_eq!(h.tutorial.state(), TutorialState::PendingContract);
    assert!(!h.calls.iter().any(|c| matches!(
        c.request,
        InstallerRequest::Pull { .. } | InstallerRequest::WindowsFrom { .. }
    )));
}
#[test]
fn stable_union_survives_e1_to_e2_to_audio_and_keeps_all_still_tracked_peers() {
    let mut h = Harness::new(TutorialRole::E1Controller);
    h.status();
    controller_end(&mut h, true);
    h.confirm(HumanConfirmation::RemotePracticeAndHud);
    h.verified();
    let first = h.tutorial.current_samples()[1]
        .values
        .keys()
        .copied()
        .collect::<Vec<_>>();
    h.next(TutorialRole::E2SourcePull, Some(peer()));
    h.status();
    h.open();
    h.confirm(HumanConfirmation::SourceMachineAndAttempt);
    h.projection(local(), true);
    h.counter("e2_source_started", 1);
    h.value["result"]["installer"]["peers"][0]["last_source_parking"] = json!("twin");
    h.status();
    end_e2(&mut h);
    h.verified();
    assert_eq!(
        h.tutorial.current_samples()[1]
            .values
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        first
    );
    let other = NodeId([0x33; 32]);
    let mut p = h.value["result"]["installer"]["peers"][0].clone();
    p["node"] = json!(other);
    p["counters"]["e1_controller_started"] = json!(321);
    h.value["result"]["installer"]["peers"]
        .as_array_mut()
        .unwrap()
        .push(p);
    h.next(TutorialRole::AudioReceiver, Some(other));
    h.status();
    assert_eq!(h.tutorial.current_samples().len(), 3);
    assert_eq!(
        h.tutorial.current_samples()[0]
            .values
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        vec![CounterId(14), CounterId(15), CounterId(16)]
    );
    assert_eq!(h.tutorial.current_samples()[2].values[&CounterId(0)], 321);
    for sample in &h.tutorial.current_samples()[1..] {
        assert_eq!(sample.values.keys().copied().collect::<Vec<_>>(), first);
    }
    let prior_time = h.tutorial.current_samples()[0].observed_at_ms;
    let effects = h.tutorial.reduce(TutorialEvent::Tick, h.now + 1).unwrap();
    let TutorialEffectKind::Core(FlowEvent::Observe { samples }) = &effects[0].kind else {
        panic!()
    };
    assert_eq!(samples.len(), 3);
    assert!(samples.iter().all(|s| s.observed_at_ms == prior_time));
}
#[test]
fn core_rejects_regression_above_proof_endpoint_and_no_later_sample_revives_it() {
    let mut h = Harness::new(TutorialRole::Menu);
    h.status();
    h.value["result"]["installer"]["settings_opened"] = json!(1);
    h.status();
    h.confirm(HumanConfirmation::TrayAndSettingsVisible);
    h.verified();
    h.value["result"]["installer"]["settings_opened"] = json!(100);
    h.status();
    h.verified();
    h.value["result"]["installer"]["settings_opened"] = json!(50);
    h.status();
    assert_eq!(h.tutorial.state(), TutorialState::Failed);
    assert_eq!(
        h.tutorial.detail().failure,
        Some(TutorialFailure::Core(FlowError::InvalidEvidence))
    );
    h.value["result"]["installer"]["settings_opened"] = json!(200);
    h.status();
    assert_ne!(h.tutorial.state(), TutorialState::Verified);
}
#[test]
fn remote_selection_is_exact_id_and_local_project_is_only_owned_fixture_window() {
    let mut h = Harness::new(TutorialRole::E2SourcePush);
    h.status();
    h.open();
    let call = h.call(&InstallerRequest::Windows);
    h.reply(
        call,
        Ok(DecodedReply::Windows(vec![LocalWindow {
            id: WindowId(101),
            app: "fixture".into(),
            title: "same label".into(),
            display: None,
            size: [1.0, 1.0],
        }])),
    );
    assert_eq!(h.tutorial.state(), TutorialState::Failed);
    assert!(
        !h.calls
            .iter()
            .any(|c| matches!(c.request, InstallerRequest::Project { .. }))
    );
    let mut h = Harness::new(TutorialRole::E2DestinationPull);
    h.status();
    let call = h.call(&InstallerRequest::WindowsFrom { peer: peer() });
    h.reply(call, Ok(DecodedReply::WindowsFrom(vec![])));
    assert_eq!(
        h.tutorial.reduce(
            TutorialEvent::User {
                attempt: AttemptId(1),
                view_revision: 17,
                action: TutorialUserAction::SelectRemoteWindow {
                    window: WindowId(100)
                }
            },
            h.now + 1
        ),
        Err(TutorialError::WrongRole)
    );
}
#[test]
fn sensitive_fixture_context_device_and_observation_text_is_redacted() {
    let sentinel = "SENSITIVE_SENTINEL";
    let mut c = context();
    c.machine_label = sentinel.into();
    c.speakers.as_mut().unwrap().device_key = sentinel.into();
    let observation = TutorialFixtureObservation::Opened {
        fixture: 10,
        pid: 1,
        window: WindowId(100),
        label: sentinel.into(),
    };
    let event = TutorialEvent::Fixture {
        attempt: AttemptId(1),
        call_id: None,
        sequence: 1,
        observed_at_ms: 1,
        result: Ok(observation.clone()),
    };
    for text in [
        format!("{c:?}"),
        format!("{observation:?}"),
        format!("{event:?}"),
        format!(
            "{:?}",
            TutorialFixtureAction::PlayTone {
                fixture: 10,
                peer: peer(),
                device_key: sentinel.into()
            }
        ),
    ] {
        assert!(!text.contains(sentinel));
    }
}
fn settings_reply(id: u64, result: Result<DecodedReply, CallFailure>, at: u64) -> AgentReply {
    AgentReply {
        id,
        observed_at_ms: at,
        source: ObservationSource::Live,
        result,
    }
}
#[test]
fn settings_returned_revision_requires_restart_consent_and_new_opaque_instance() {
    let v = value();
    let h = health(&v, AgentPlatform::Linux);
    let mut s = SettingsTransition::new(local(), 17);
    s.track_peers(&[peer()]).unwrap();
    let detected = s.detected(&h, ObservationSource::Live, 1, 1, 17).unwrap();
    let FlowEvent::Observe { samples } = &detected[0] else {
        panic!()
    };
    assert_eq!(samples.len(), 2);
    assert!(samples.iter().all(|s| s.observed_at_ms == 1));
    let update = s.consent_update(100, 17, true).unwrap();
    assert_eq!(
        update.request,
        InstallerRequest::SettingsUpdate {
            expected_revision: "1111111111111111".into(),
            mac_virtual_display: true
        }
    );
    assert_eq!(
        s.reply(
            settings_reply(
                100,
                Ok(DecodedReply::SettingsUpdated(SettingsUpdated {
                    revision: "2222222222222222".into(),
                    restart_required: true
                })),
                2
            ),
            2
        ),
        Ok(SettingsOutcome {
            observations: Vec::new(),
            detect_after_unknown: false
        })
    );
    assert_eq!(s.returned_revision(), Some("2222222222222222"));
    assert_eq!(s.state(), &SettingsTransitionState::NeedsRestartConsent);
    let samples = samples.clone();
    let (events, restart) = s
        .consent_restart(101, 17, &[StepId(1), StepId(2)], &samples)
        .unwrap();
    assert_eq!(restart.request, InstallerRequest::Restart);
    assert_eq!(
        events,
        vec![
            FlowEvent::Observe { samples },
            FlowEvent::Invalidate { step: StepId(1) },
            FlowEvent::Invalidate { step: StepId(2) }
        ]
    );
    s.reply(settings_reply(101, Ok(DecodedReply::Acknowledged), 3), 3)
        .unwrap();
    let mut old = v.clone();
    old["result"]["installer"]["config_revision"] = json!("2222222222222222");
    for (id, instance, revision) in [
        (102, 99, "2222222222222222"),
        (103, 100, "1111111111111111"),
        (104, u64::MAX, "2222222222222222"),
    ] {
        old["result"]["installer"]["instance"]["id"] = json!(instance);
        old["result"]["installer"]["config_revision"] = json!(revision);
        s.poll_new_instance(id).unwrap();
        let outcome = s
            .reply(
                settings_reply(
                    id,
                    Ok(DecodedReply::Status(StatusAdmission::Supported(health(
                        &old,
                        AgentPlatform::Linux,
                    )))),
                    id,
                ),
                id,
            )
            .unwrap();
        let FlowEvent::Observe { samples } = &outcome.observations[0] else {
            panic!()
        };
        assert_eq!(samples.len(), 2);
        assert!(
            samples
                .iter()
                .all(|s| s.observed_at_ms == id && s.binding.epochs.instance_id == instance)
        );
        assert_eq!(
            s.state(),
            if id == 104 {
                &SettingsTransitionState::Complete
            } else {
                &SettingsTransitionState::WaitingNewInstance
            }
        );
    }
}
#[test]
fn settings_conflict_and_unknown_mutation_require_detection_and_renewed_view_consent() {
    for error in [
        CallFailure::Refused(AgentRefusal::RevisionConflict),
        CallFailure::TimeoutOutcomeUnknown,
    ] {
        let h = health(&value(), AgentPlatform::Linux);
        let mut s = SettingsTransition::new(local(), 17);
        s.detected(&h, ObservationSource::Live, 1, 1, 17).unwrap();
        s.consent_update(1, 17, true).unwrap();
        assert_eq!(
            s.reply(settings_reply(1, Err(error.clone()), 2), 2),
            Ok(SettingsOutcome {
                observations: Vec::new(),
                detect_after_unknown: true
            })
        );
        assert_eq!(s.state(), &SettingsTransitionState::NeedsDetection);
        assert!(s.consent_update(2, 17, true).is_err());
        assert!(s.detected(&h, ObservationSource::Live, 3, 3, 17).is_err());
        s.detected(&h, ObservationSource::Live, 3, 3, 18).unwrap();
        assert!(s.consent_update(2, 17, true).is_err());
        if error == CallFailure::Refused(AgentRefusal::RevisionConflict) {
            assert_eq!(
                s.state(),
                &SettingsTransitionState::NeedsRecoveryRestartConsent
            );
            assert!(s.consent_update(2, 18, true).is_err());
            continue;
        }
        assert_eq!(
            s.consent_update(2, 18, true).unwrap().request,
            InstallerRequest::SettingsUpdate {
                expected_revision: "1111111111111111".into(),
                mac_virtual_display: true
            }
        );
    }
}
#[test]
fn settings_refusal_invalid_response_stale_reply_and_demo_cannot_complete() {
    for result in [
        Err(CallFailure::Refused(AgentRefusal::NotSupported)),
        Ok(DecodedReply::Acknowledged),
        Ok(DecodedReply::SettingsUpdated(SettingsUpdated {
            revision: "2222222222222222".into(),
            restart_required: false,
        })),
    ] {
        let mut s = SettingsTransition::new(local(), 17);
        s.detected(
            &health(&value(), AgentPlatform::Linux),
            ObservationSource::Live,
            1,
            1,
            17,
        )
        .unwrap();
        s.consent_update(1, 17, true).unwrap();
        let _ = s.reply(settings_reply(1, result, 2), 2);
        assert!(matches!(s.state(), SettingsTransitionState::Failed(_)));
    }
    let mut s = SettingsTransition::new(local(), 17);
    let h = health(&value(), AgentPlatform::Linux);
    assert!(s.detected(&h, ObservationSource::Demo, 1, 1, 17).is_err());
    s.detected(&h, ObservationSource::Live, 1, 1, 17).unwrap();
    s.consent_update(1, 17, true).unwrap();
    let mut demo = settings_reply(1, Ok(DecodedReply::Acknowledged), 2);
    demo.source = ObservationSource::Demo;
    assert!(s.reply(demo, 2).is_err());
    assert!(
        s.reply(settings_reply(1, Ok(DecodedReply::Acknowledged), 100), 2)
            .is_err()
    );
}
#[test]
fn fixture_loss_after_end_retires_restoration_and_not_owned_never_closes_child() {
    let mut h = start_e2(TutorialRole::E2SourcePull);
    h.projection(local(), false);
    h.counter("e2_source_returned", 1);
    h.value["result"]["installer"]["recovery_pending"] = json!(0);
    h.status();
    let id = h.fixture_action(|a| matches!(a, TutorialFixtureAction::ObserveWindow { .. }));
    h.fixture(
        Some(id),
        Ok(snapshot(None, 0, TutorialWindowFacts::Missing)),
    );
    assert_eq!(h.tutorial.state(), TutorialState::Failed);
    assert_eq!(
        h.tutorial.detail().failure,
        Some(TutorialFailure::Fixture(
            TutorialFixtureError::UnknownWindow
        ))
    );
    let mut h = audio_start(true);
    h.fixture(None, Err(TutorialFixtureError::NotOwned));
    assert_eq!(h.tutorial.state(), TutorialState::Failed);
    assert!(h.fixtures.iter().all(|(_, a)| !matches!(
        a,
        TutorialFixtureAction::StopTone { .. } | TutorialFixtureAction::Close { .. }
    )));
    assert!(!h.tutorial.detail().cleanup_settled);
}
#[test]
fn source_return_failure_wrong_home_facts_and_extra_projection_sessions_do_not_pass() {
    let mut h = start_e2(TutorialRole::E2SourcePull);
    h.counter("e2_returns_failed", 1);
    h.status();
    assert_eq!(h.tutorial.state(), TutorialState::Failed);
    for facts in [
        TutorialWindowFacts::Unknown,
        TutorialWindowFacts::Present {
            visible_on_user_workspace: Some(false),
            on_initial_display: Some(true),
        },
        TutorialWindowFacts::Present {
            visible_on_user_workspace: Some(true),
            on_initial_display: None,
        },
    ] {
        let mut h = start_e2(TutorialRole::E2SourcePull);
        h.confirm(HumanConfirmation::DestinationPatternInteractionAndClose);
        h.ack(InstallerRequest::Return {
            projection: 55,
            source: None,
        });
        h.projection(local(), false);
        h.counter("e2_source_returned", 1);
        h.value["result"]["installer"]["recovery_pending"] = json!(0);
        h.status();
        let id = h.fixture_action(|a| matches!(a, TutorialFixtureAction::ObserveWindow { .. }));
        h.fixture(Some(id), Ok(snapshot(None, 0, facts)));
        assert_ne!(h.tutorial.state(), TutorialState::Verified);
        assert!(h.fixtures.is_empty());
    }
    let mut h = start_e2(TutorialRole::E2DestinationPush);
    h.counter("e2_dest_started", 2);
    h.status();
    assert_eq!(h.tutorial.state(), TutorialState::Failed);
}
#[test]
fn receiver_does_not_baseline_before_selected_source_explicit_tone_attestation() {
    let mut h = Harness::new(TutorialRole::AudioReceiver);
    h.status();
    h.value["result"]["installer"]["audio"]["active_peers"] = json!([peer()]);
    h.status();
    h.value["result"]["installer"]["audio"]["frames_played"] = json!(10);
    h.status();
    h.confirm(HumanConfirmation::LocalSpeakerHeard);
    h.confirm(HumanConfirmation::ExclusiveAudioInterval);
    assert_ne!(h.tutorial.state(), TutorialState::Verified);
    h.confirm(HumanConfirmation::SelectedSourceToneStarted);
    h.status();
    h.value["result"]["installer"]["audio"]["frames_played"] = json!(20);
    h.status();
    h.verified();
    assert!(h.observed.iter().all(|e| !matches!(
        e.kind,
        TutorialEffectKind::Fixture {
            action: TutorialFixtureAction::PlayTone { .. },
            ..
        }
    )));
}
#[test]
fn adapter_checks_expired_core_prerequisite_before_dispatching_fixture() {
    let specs = vec![
        StepSpec {
            id: StepId(99),
            prerequisites: vec![],
            required_for_installed: true,
            required_for_ready: true,
            requires_fresh_observation: true,
            requires_activity: false,
            requires_human: false,
            requires_fixture: false,
        },
        StepSpec {
            id: StepId(1),
            prerequisites: vec![StepId(99)],
            required_for_installed: false,
            required_for_ready: false,
            requires_fresh_observation: false,
            requires_activity: true,
            requires_human: true,
            requires_fixture: true,
        },
    ];
    let mut core = Flow::new(specs).unwrap();
    let h = health(&value(), AgentPlatform::Linux);
    let sample = counter_sample(&h, None, ObservationSource::Live, 1)
        .unwrap()
        .sample;
    core.reduce(
        FlowEvent::Observe {
            samples: vec![sample.clone()],
        },
        1,
    )
    .unwrap();
    let detect = core
        .reduce(FlowEvent::Begin { step: StepId(99) }, 1)
        .unwrap()
        .remove(0);
    let verify = core
        .reduce(
            FlowEvent::Detected {
                step: StepId(99),
                operation: detect.operation,
                needs_action: false,
            },
            1,
        )
        .unwrap()
        .remove(0);
    core.reduce(
        FlowEvent::Verified {
            step: StepId(99),
            operation: verify.operation,
            verification: Verification {
                source: ObservationSource::Live,
                observed_at_ms: 1,
                binding: Some(sample.binding),
                activity: None,
                human_attempt: None,
                fixture_attempt: None,
            },
        },
        1,
    )
    .unwrap();
    let detect = core
        .reduce(FlowEvent::Begin { step: StepId(1) }, 2)
        .unwrap()
        .remove(0);
    let verify = core
        .reduce(
            FlowEvent::Detected {
                step: StepId(1),
                operation: detect.operation,
                needs_action: false,
            },
            2,
        )
        .unwrap()
        .remove(0);
    let mut t = Tutorial::new();
    let attempt = TutorialAttempt {
        step: StepId(1),
        operation: verify.operation,
        attempt: AttemptId(1),
        local: local(),
        peer: Some(peer()),
        role: TutorialRole::E1Target,
    };
    let effects = t.begin(attempt.clone(), context(), verify, 17, 2).unwrap();
    let id = effects
        .iter()
        .find_map(|e| match &e.kind {
            TutorialEffectKind::Agent(c) => Some(c.id),
            _ => None,
        })
        .unwrap();
    let effects = t
        .reduce(
            TutorialEvent::Reply(settings_reply(
                id,
                Ok(DecodedReply::Status(StatusAdmission::Supported(h))),
                6000,
            )),
            6000,
        )
        .unwrap();
    let TutorialEffectKind::Core(observe) = &effects[0].kind else {
        panic!()
    };
    core.reduce(observe.clone(), 6000).unwrap();
    assert_eq!(
        core.summary(6000, t.current_samples())
            .steps
            .iter()
            .find(|s| s.id == StepId(1))
            .unwrap()
            .state,
        StepState::Stale
    );
    // The adapter stops the batch before dispatching Open; feedback retires the operation.
    let cleanup = t
        .reduce(
            TutorialEvent::CoreRejected(FlowError::PrerequisitePending),
            6000,
        )
        .unwrap();
    assert!(cleanup.iter().all(|e| !matches!(
        e.kind,
        TutorialEffectKind::Fixture {
            action: TutorialFixtureAction::Open { .. },
            ..
        }
    )));
    assert_eq!(t.state(), TutorialState::Failed);
    let mut h = Harness {
        tutorial: t,
        core,
        port: AgentQueue::default(),
        calls: vec![],
        fixtures: vec![],
        observed: vec![],
        value: value(),
        now: 6000,
        sequence: 0,
        attempt,
        platform: AgentPlatform::Linux,
        binding: None,
        ids: Default::default(),
        core_errors: vec![],
        retired: Default::default(),
    };
    h.consume(cleanup);
    h.ack(InstallerRequest::Release);
    h.status();
    assert!(h.tutorial.detail().cleanup_settled);
    assert!(
        h.fixtures.is_empty(),
        "the discarded Open was never submitted"
    );
    let local_sample = h
        .tutorial
        .current_samples()
        .iter()
        .find(|s| s.binding.peer.is_none())
        .unwrap()
        .clone();
    let detect = h
        .core
        .reduce(FlowEvent::Begin { step: StepId(99) }, h.now)
        .unwrap()
        .remove(0);
    let verify = h
        .core
        .reduce(
            FlowEvent::Detected {
                step: StepId(99),
                operation: detect.operation,
                needs_action: false,
            },
            h.now,
        )
        .unwrap()
        .remove(0);
    h.core
        .reduce(
            FlowEvent::Verified {
                step: StepId(99),
                operation: verify.operation,
                verification: Verification {
                    source: ObservationSource::Live,
                    observed_at_ms: local_sample.observed_at_ms,
                    binding: Some(local_sample.binding),
                    activity: None,
                    human_attempt: None,
                    fixture_attempt: None,
                },
            },
            h.now,
        )
        .unwrap();
    let detect = h
        .core
        .reduce(FlowEvent::Begin { step: StepId(1) }, h.now)
        .unwrap()
        .remove(0);
    let verify = h
        .core
        .reduce(
            FlowEvent::Detected {
                step: StepId(1),
                operation: detect.operation,
                needs_action: false,
            },
            h.now,
        )
        .unwrap()
        .remove(0);
    h.attempt.operation = verify.operation;
    h.attempt.attempt = AttemptId(2);
    h.binding = None;
    let effects = h
        .tutorial
        .begin(h.attempt.clone(), context(), verify, 17, h.now)
        .unwrap();
    h.consume(effects);
    h.status();
    h.open();
    let arm = h.fixture_action(|a| matches!(a, TutorialFixtureAction::ArmTarget { phase: 2, .. }));
    h.fixture(
        Some(arm),
        Ok(TutorialFixtureObservation::TargetArmed {
            fixture: 10,
            phase: 2,
        }),
    );
    h.fixture(None, Ok(snapshot(Some(2), 1, TutorialWindowFacts::Unknown)));
    for name in [
        "e1_target_started",
        "e1_target_ended",
        "e1_injections_ok",
        "e1_hud_shows",
    ] {
        h.counter(name, 1);
    }
    h.status();
    h.confirm(HumanConfirmation::ControllerCrossingAndRelease);
    h.close();
    h.verified();
}
#[test]
fn cancelled_source_requires_fresh_home_before_child_close_or_cleanup_settlement() {
    let mut h = start_e2(TutorialRole::E2SourcePull);
    h.event(TutorialEvent::User {
        attempt: AttemptId(1),
        view_revision: 17,
        action: TutorialUserAction::Cancel,
    });
    h.ack(InstallerRequest::Return {
        projection: 55,
        source: None,
    });
    h.projection(local(), false);
    h.counter("e2_source_returned", 1);
    h.value["result"]["installer"]["recovery_pending"] = json!(0);
    h.status();
    assert!(
        !h.fixtures
            .iter()
            .any(|(_, a)| matches!(a, TutorialFixtureAction::Close { .. })),
        "terminal routing is not source restoration"
    );
    assert!(!h.tutorial.detail().cleanup_settled);
    let id =
        h.fixture_action(|a| matches!(a, TutorialFixtureAction::ObserveWindow { fixture: 10 }));
    h.fixture(
        Some(id),
        Ok(snapshot(None, 0, TutorialWindowFacts::Unknown)),
    );
    assert!(
        !h.fixtures
            .iter()
            .any(|(_, a)| matches!(a, TutorialFixtureAction::Close { .. }))
    );
    assert!(!h.tutorial.detail().cleanup_settled);
}
#[test]
fn cancelled_source_failed_return_keeps_child_and_recovery_uncertain() {
    let mut h = start_e2(TutorialRole::E2SourcePull);
    h.event(TutorialEvent::User {
        attempt: AttemptId(1),
        view_revision: 17,
        action: TutorialUserAction::Cancel,
    });
    h.ack(InstallerRequest::Return {
        projection: 55,
        source: None,
    });
    h.projection(local(), false);
    h.counter("e2_returns_failed", 1);
    h.value["result"]["installer"]["recovery_pending"] = json!(0);
    h.status();
    assert!(
        !h.fixtures
            .iter()
            .any(|(_, a)| matches!(a, TutorialFixtureAction::Close { .. })),
        "failed restoration cannot discard the child"
    );
    assert!(!h.tutorial.detail().cleanup_settled);
    assert_eq!(h.tutorial.state(), TutorialState::Failed);
}
#[test]
fn cancelled_source_fresh_restored_owned_window_and_closed_child_settle() {
    let mut h = start_e2(TutorialRole::E2SourcePull);
    h.event(TutorialEvent::User {
        attempt: AttemptId(1),
        view_revision: 17,
        action: TutorialUserAction::Cancel,
    });
    h.ack(InstallerRequest::Return {
        projection: 55,
        source: None,
    });
    h.projection(local(), false);
    h.counter("e2_source_returned", 1);
    h.value["result"]["installer"]["recovery_pending"] = json!(0);
    h.status();
    h.home();
    assert!(!h.tutorial.detail().cleanup_settled);
    h.close();
    assert!(h.tutorial.detail().cleanup_settled);
    assert_eq!(h.tutorial.state(), TutorialState::Cancelled);
    assert!(
        h.observed
            .iter()
            .all(|e| !matches!(e.kind, TutorialEffectKind::Core(FlowEvent::Verified { .. })))
    );
}
#[test]
fn shared_call_handoff_settings_to_tutorial_is_monotonic_and_refuses_busy_or_exhaustion() {
    let mut s = SettingsTransition::new(local(), 17);
    s.detected(
        &health(&value(), AgentPlatform::Linux),
        ObservationSource::Live,
        1,
        1,
        17,
    )
    .unwrap();
    s.consent_update(50, 17, true).unwrap();
    assert!(s.consent_update(51, 17, true).is_err());
    s.reply(
        settings_reply(50, Err(CallFailure::Refused(AgentRefusal::NotSupported)), 2),
        2,
    )
    .unwrap();
    let mut t = Tutorial::new();
    t.advance_call_ids(s.last_call_id()).unwrap();
    let attempt = TutorialAttempt {
        step: StepId(1),
        operation: OperationId(1),
        attempt: AttemptId(1),
        local: local(),
        peer: None,
        role: TutorialRole::Menu,
    };
    let effects = t
        .begin(
            attempt,
            context(),
            JobIntent {
                step: StepId(1),
                operation: OperationId(1),
                stage: JobStage::Verify,
            },
            17,
            3,
        )
        .unwrap();
    let call = effects
        .iter()
        .find_map(|e| match &e.kind {
            TutorialEffectKind::Agent(c) => Some(c),
            _ => None,
        })
        .unwrap();
    assert_eq!(call.id, 51);
    assert_eq!(t.last_call_id(), 51);
    assert_eq!(t.advance_call_ids(60), Err(TutorialError::Busy));
    assert_eq!(
        Tutorial::new().advance_call_ids(u64::MAX),
        Err(TutorialError::IdExhausted)
    );
    let mut s = SettingsTransition::new(local(), 17);
    s.detected(
        &health(&value(), AgentPlatform::Linux),
        ObservationSource::Live,
        3,
        3,
        17,
    )
    .unwrap();
    assert_eq!(
        s.consent_update(t.last_call_id() + 1, 17, true).unwrap().id,
        52
    );
    assert!(s.consent_update(u64::MAX, 17, true).is_err());
}

fn cancel(h: &mut Harness) {
    h.event(TutorialEvent::User {
        attempt: h.attempt.attempt,
        view_revision: 17,
        action: TutorialUserAction::Cancel,
    });
}
fn no_close(h: &Harness) {
    assert!(
        !h.fixtures
            .iter()
            .any(|(_, a)| matches!(a, TutorialFixtureAction::Close { .. }))
    );
    assert!(!h.tutorial.detail().cleanup_settled);
    assert_ne!(h.tutorial.state(), TutorialState::Verified);
}
fn preparation(sender: bool) -> Harness {
    let mut h = Harness::new(if sender {
        TutorialRole::AudioSender
    } else {
        TutorialRole::AudioReceiver
    });
    h.status();
    if sender {
        h.open();
    } else {
        h.confirm(HumanConfirmation::SelectedSourceToneStarted);
    }
    h
}
fn play(h: &mut Harness) -> u64 {
    h.event(TutorialEvent::User {
        attempt: h.attempt.attempt,
        view_revision: 17,
        action: TutorialUserAction::PlayTestSound,
    });
    h.fixture_action(|a| {
        *a == TutorialFixtureAction::PlayTone {
            fixture: 10,
            peer: peer(),
            device_key: "validated virtual output".into(),
        }
    })
}
fn tone_started(h: &mut Harness, id: u64) {
    h.fixture(
        Some(id),
        Ok(TutorialFixtureObservation::ToneStarted {
            fixture: 10,
            tone: 33,
        }),
    );
}

#[test]
fn admitted_audio_binding_retires_sender_and_receiver_before_baseline() {
    for sender in [false, true] {
        for path in ["gate", "grants", "backends", "instance", "link"] {
            let mut h = preparation(sender);
            if sender {
                let id = play(&mut h);
                tone_started(&mut h, id);
            }
            match path {
                "instance" => h.value["result"]["installer"]["instance"]["id"] = json!(100),
                "link" => h.value["result"]["installer"]["peers"][0]["link_generation"] = json!(3),
                p => h.value["result"]["installer"]["epochs"][p] = json!(2),
            }
            h.value["result"]["installer"]["audio"]["active_peers"] = json!([peer()]);
            h.status();
            assert_eq!(h.tutorial.state(), TutorialState::Failed);
            assert_eq!(
                h.tutorial.detail().failure,
                Some(TutorialFailure::BindingChanged)
            );
            if sender {
                assert!(h.fixtures.iter().any(|(_, a)| *a
                    == TutorialFixtureAction::StopTone {
                        fixture: 10,
                        tone: 33
                    }));
                no_close(&h);
            }
            assert!(
                !h.observed.iter().any(|e| matches!(
                    e.kind,
                    TutorialEffectKind::Core(FlowEvent::Verified { .. })
                ))
            );
        }
    }
}
#[test]
fn audio_delayed_pre_action_receipts_cannot_supply_either_baseline() {
    for sender in [false, true] {
        let mut h = Harness::new(if sender {
            TutorialRole::AudioSender
        } else {
            TutorialRole::AudioReceiver
        });
        h.status();
        if sender {
            h.open();
        }
        h.event(TutorialEvent::Tick);
        let call = h.call(&InstallerRequest::Status);
        h.now += 1;
        let at = h.now;
        h.value["result"]["installer"]["audio"]["active_peers"] = json!([peer()]);
        let reply = settings_reply(
            call.id,
            Ok(DecodedReply::Status(StatusAdmission::Supported(health(
                &h.value, h.platform,
            )))),
            at,
        );
        if sender {
            let id = play(&mut h);
            tone_started(&mut h, id);
        } else {
            h.confirm(HumanConfirmation::SelectedSourceToneStarted);
        }
        h.event(TutorialEvent::Reply(reply));
        h.value["result"]["installer"]["audio"][if sender {
            "frames_sent"
        } else {
            "frames_played"
        }] = json!(10);
        h.status(); // this later receipt is the baseline, not an endpoint
        h.confirm(if sender {
            HumanConfirmation::FarSpeakerHeard
        } else {
            HumanConfirmation::LocalSpeakerHeard
        });
        h.confirm(HumanConfirmation::ExclusiveAudioInterval);
        assert_ne!(h.tutorial.state(), TutorialState::Verified);
        assert!(
            !h.fixtures
                .iter()
                .any(|(_, a)| matches!(a, TutorialFixtureAction::StopTone { .. }))
        );
    }
}
#[test]
fn owned_natural_tone_expiry_retires_without_restarting_playback() {
    let mut h = audio_start(true);
    h.fixture(
        None,
        Ok(TutorialFixtureObservation::ToneStopped {
            fixture: 10,
            tone: 33,
        }),
    );
    assert_eq!(h.tutorial.state(), TutorialState::Failed);
    assert!(!h.fixtures.iter().any(|(_, a)| matches!(
        a,
        TutorialFixtureAction::PlayTone { .. } | TutorialFixtureAction::StopTone { .. }
    )));
    h.close();
    assert!(!h.tutorial.detail().cleanup_settled);
    h.ack(InstallerRequest::Release);
    h.value["result"]["installer"]["audio"]["active_peers"] = json!([]);
    h.status();
    assert!(h.tutorial.detail().cleanup_settled);
    assert_eq!(h.tutorial.state(), TutorialState::Failed);
}
#[test]
fn earlier_tone_stop_receipt_cannot_settle_later_agent_endpoint() {
    let mut h = audio_start(true);
    let stop_at = h.now + 1;
    h.now += 2;
    h.value["result"]["installer"]["audio"]["frames_sent"] = json!(10);
    h.status();
    h.sequence += 1;
    h.event(TutorialEvent::Fixture {
        attempt: h.attempt.attempt,
        call_id: None,
        sequence: h.sequence,
        observed_at_ms: stop_at,
        result: Ok(TutorialFixtureObservation::ToneStopped {
            fixture: 10,
            tone: 33,
        }),
    });
    assert_eq!(h.tutorial.state(), TutorialState::Failed);
    h.close();
    assert_ne!(h.tutorial.state(), TutorialState::Verified);
}
#[test]
fn native_owned_close_request_retires_then_waits_for_closed_outcome() {
    let mut h = Harness::new(TutorialRole::E1Target);
    h.status();
    h.open();
    let arm = h.fixture_action(|a| matches!(a, TutorialFixtureAction::ArmTarget { .. }));
    h.fixture(
        Some(arm),
        Ok(TutorialFixtureObservation::TargetArmed {
            fixture: 10,
            phase: 1,
        }),
    );
    h.fixture(
        None,
        Ok(TutorialFixtureObservation::CloseRequested { fixture: 10 }),
    );
    assert_eq!(h.tutorial.state(), TutorialState::Cancelled);
    assert!(!h.tutorial.detail().cleanup_settled);
    h.ack(InstallerRequest::Release);
    h.status();
    h.close();
    assert!(h.tutorial.detail().cleanup_settled);
}
#[test]
fn complete_e1_then_incomplete_e2_observes_absence_for_previous_proof() {
    let mut h = Harness::new(TutorialRole::E1Controller);
    h.status();
    controller_end(&mut h, true);
    h.confirm(HumanConfirmation::RemotePracticeAndHud);
    h.verified();
    h.next(TutorialRole::E2SourcePull, Some(peer()));
    let call = h.call(&InstallerRequest::Status);
    h.reply(
        call,
        Ok(DecodedReply::Status(
            StatusAdmission::PendingHealthContract(PendingHealthReason::Incomplete),
        )),
    );
    assert!(h.tutorial.current_samples().is_empty());
    assert_eq!(
        h.core
            .summary(h.now, h.tutorial.current_samples())
            .steps
            .iter()
            .find(|s| s.id == StepId(1))
            .unwrap()
            .state,
        StepState::Stale
    );
    assert_eq!(h.tutorial.state(), TutorialState::PendingContract);
}
#[test]
fn target_each_counter_armed_phase_click_and_confirmation_are_independent() {
    for missing in [
        "e1_target_started",
        "e1_target_ended",
        "e1_injections_ok",
        "e1_hud_shows",
        "armed",
        "phase",
        "click",
        "human",
    ] {
        let mut h = Harness::new(TutorialRole::E1Target);
        h.status();
        h.open();
        if missing != "armed" {
            let arm = h.fixture_action(|a| matches!(a, TutorialFixtureAction::ArmTarget { .. }));
            h.fixture(
                Some(arm),
                Ok(TutorialFixtureObservation::TargetArmed {
                    fixture: 10,
                    phase: 1,
                }),
            );
        }
        h.fixture(
            None,
            Ok(snapshot(
                if missing == "phase" { None } else { Some(1) },
                if missing == "click" { 0 } else { 1 },
                TutorialWindowFacts::Unknown,
            )),
        );
        for name in [
            "e1_target_started",
            "e1_target_ended",
            "e1_injections_ok",
            "e1_hud_shows",
        ] {
            if name != missing {
                h.counter(name, 1);
            }
        }
        h.status();
        if missing != "human" {
            h.confirm(HumanConfirmation::ControllerCrossingAndRelease);
        }
        assert_ne!(
            h.tutorial.state(),
            TutorialState::Verified,
            "missing {missing}"
        );
        assert!(
            !h.fixtures
                .iter()
                .any(|(_, a)| matches!(a, TutorialFixtureAction::Close { .. }))
        );
    }
}
#[test]
fn menu_counter_with_missing_tray_or_human_never_verifies() {
    for missing_tray in [false, true] {
        let mut h = Harness::new(TutorialRole::Menu);
        h.status();
        h.value["result"]["installer"]["settings_opened"] = json!(1);
        h.value["result"]["installer"]["tray"]["created"] = json!(!missing_tray);
        h.status();
        if missing_tray && h.tutorial.state() != TutorialState::Failed {
            h.confirm(HumanConfirmation::TrayAndSettingsVisible);
        }
        assert_ne!(h.tutorial.state(), TutorialState::Verified);
    }
}
#[test]
fn all_four_e2_roles_require_observed_owned_projection() {
    for role in [
        TutorialRole::E2SourcePush,
        TutorialRole::E2SourcePull,
        TutorialRole::E2DestinationPush,
        TutorialRole::E2DestinationPull,
    ] {
        let mut h = Harness::new(role);
        h.status();
        authorize_e2(&mut h);
        let source = matches!(
            role,
            TutorialRole::E2SourcePush | TutorialRole::E2SourcePull
        );
        h.counter(
            if source {
                "e2_source_started"
            } else {
                "e2_dest_started"
            },
            1,
        );
        h.counter(
            if source {
                "e2_source_returned"
            } else {
                "e2_dest_returned"
            },
            1,
        );
        if source {
            h.value["result"]["installer"]["peers"][0]["last_source_parking"] = json!("twin");
        } else {
            h.counter("e2_frames_presented", 10);
        }
        h.status();
        h.confirm(HumanConfirmation::DestinationPatternInteractionAndClose);
        if source {
            h.fixture(
                None,
                Ok(snapshot(
                    None,
                    0,
                    TutorialWindowFacts::Present {
                        visible_on_user_workspace: Some(true),
                        on_initial_display: Some(true),
                    },
                )),
            );
        } else {
            h.confirm(HumanConfirmation::SourceRestored);
        }
        assert_ne!(h.tutorial.state(), TutorialState::Verified);
        assert!(
            !h.observed
                .iter()
                .any(|e| matches!(e.kind, TutorialEffectKind::Core(FlowEvent::Verified { .. })))
        );
    }
}
#[test]
fn complete_push_pull_destinations_require_presented_advance_return_success_and_source_human() {
    for role in [
        TutorialRole::E2DestinationPush,
        TutorialRole::E2DestinationPull,
    ] {
        for missing in ["presented", "returned", "source_human"] {
            let mut h = start_e2(role);
            h.value["result"]["projections"][0]["received"]["frames"] = json!(u64::MAX);
            h.value["result"]["projections"][0]["received"]["bytes"] = json!(u64::MAX);
            h.status();
            h.confirm(HumanConfirmation::DestinationPatternInteractionAndClose);
            h.ack(InstallerRequest::Return {
                projection: 55,
                source: Some(peer()),
            });
            h.projection(peer(), false);
            h.counter("e2_dest_returned", 1);
            if missing != "presented" {
                h.counter("e2_frames_presented", 10);
            }
            if missing == "returned" {
                h.counter("e2_returns_failed", 1);
                h.counter("e2_dest_returned", 0);
            }
            h.status();
            if missing != "source_human" && h.tutorial.state() != TutorialState::Failed {
                h.confirm(HumanConfirmation::SourceRestored);
            }
            assert_ne!(
                h.tutorial.state(),
                TutorialState::Verified,
                "{role:?} missing {missing}"
            );
            assert!(
                h.observed
                    .iter()
                    .any(|e| matches!(e.kind, TutorialEffectKind::Core(FlowEvent::Observe { .. })))
            );
            // The active projection above has populated received/decoder statistics, yet no presented advance is invented.
        }
    }
}

fn blocked_replacement(h: &mut Harness) {
    let mut next = h.attempt.clone();
    next.attempt.0 += 100;
    next.operation.0 += 100;
    let job = JobIntent {
        step: next.step,
        operation: next.operation,
        stage: JobStage::Verify,
    };
    assert_eq!(
        h.tutorial.begin(next, context(), job, 18, h.now),
        Err(TutorialError::Busy)
    );
}
#[test]
fn cancellation_during_open_and_arm_keeps_submitted_outcomes_and_blocks_replacement() {
    for during_arm in [false, true] {
        let mut h = Harness::new(TutorialRole::E1Target);
        h.status();
        if during_arm {
            h.open();
        }
        cancel(&mut h);
        no_close(&h);
        blocked_replacement(&mut h);
        if during_arm {
            let id = h.fixture_action(|a| matches!(a, TutorialFixtureAction::ArmTarget { .. }));
            h.fixture(
                Some(id),
                Ok(TutorialFixtureObservation::TargetArmed {
                    fixture: 10,
                    phase: 1,
                }),
            );
        } else {
            h.open();
        }
        assert_eq!(
            h.fixtures
                .iter()
                .filter(|(_, a)| *a == TutorialFixtureAction::Close { fixture: 10 })
                .count(),
            1
        );
        assert!(!h.tutorial.detail().cleanup_settled);
        h.ack(InstallerRequest::Release);
        h.status();
        h.close();
        assert!(h.tutorial.detail().cleanup_settled);
        assert_eq!(h.tutorial.state(), TutorialState::Cancelled);
    }
}
#[test]
fn cancellation_during_play_waits_for_late_owned_tone_then_observed_stop() {
    let mut h = preparation(true);
    let id = play(&mut h);
    cancel(&mut h);
    no_close(&h);
    blocked_replacement(&mut h);
    tone_started(&mut h, id);
    no_close(&h);
    let stop = h.fixture_action(|a| {
        *a == TutorialFixtureAction::StopTone {
            fixture: 10,
            tone: 33,
        }
    });
    h.fixture(
        Some(stop),
        Ok(TutorialFixtureObservation::ToneStopped {
            fixture: 10,
            tone: 33,
        }),
    );
    assert!(!h.tutorial.detail().cleanup_settled);
    h.ack(InstallerRequest::Release);
    h.status();
    h.close();
    assert!(h.tutorial.detail().cleanup_settled);
    assert_eq!(h.tutorial.state(), TutorialState::Cancelled);
    assert!(
        !h.observed
            .iter()
            .any(|e| matches!(e.kind, TutorialEffectKind::Core(FlowEvent::Verified { .. })))
    );
}
fn pending_projection(role: TutorialRole) -> (Harness, AgentCall) {
    let mut h = Harness::new(role);
    h.status();
    if role == TutorialRole::E2SourcePush {
        h.open();
        let call = h.call(&InstallerRequest::Windows);
        h.reply(call, Ok(decode_reply(&InstallerRequest::Windows, br#"{"ok":true,"result":[{"id":100,"app":"fixture","title":"fixture","display":null,"size":[20.5,30.5]}]}"#, h.platform).unwrap()));
        let call = h.call(&InstallerRequest::Project {
            window: WindowId(100),
            peer: peer(),
        });
        (h, call)
    } else {
        let call = h.call(&InstallerRequest::WindowsFrom { peer: peer() });
        h.reply(call, Ok(decode_reply(&InstallerRequest::WindowsFrom { peer: peer() }, br#"{"ok":true,"result":[{"id":100,"app":"fixture","title":"fixture","size":[20,30]}]}"#, h.platform).unwrap()));
        h.event(TutorialEvent::User {
            attempt: h.attempt.attempt,
            view_revision: 17,
            action: TutorialUserAction::SelectRemoteWindow {
                window: WindowId(100),
            },
        });
        h.confirm(HumanConfirmation::SourceMachineAndAttempt);
        let call = h.call(&InstallerRequest::Pull {
            peer: peer(),
            window: WindowId(100),
        });
        (h, call)
    }
}
#[test]
fn cancelled_or_unknown_project_pull_detects_late_owned_projection_before_cleanup() {
    for role in [TutorialRole::E2SourcePush, TutorialRole::E2DestinationPull] {
        for timeout in [false, true] {
            let (mut h, command) = pending_projection(role);
            let source = role == TutorialRole::E2SourcePush;
            if timeout {
                h.reply(command.clone(), Err(CallFailure::TimeoutOutcomeUnknown));
            } else {
                cancel(&mut h);
            }
            no_close(&h);
            blocked_replacement(&mut h);
            h.status();
            no_close(&h); // absence while the submitted command is unresolved cannot settle
            h.projection(if source { local() } else { peer() }, true);
            h.counter(
                if source {
                    "e2_source_started"
                } else {
                    "e2_dest_started"
                },
                1,
            );
            if source {
                h.value["result"]["installer"]["recovery_pending"] = json!(1);
            }
            h.status();
            no_close(&h);
            assert!(h.calls.iter().any(|c| c.request
                == InstallerRequest::Return {
                    projection: 55,
                    source: (!source).then_some(peer())
                }));
            if !timeout {
                h.reply(command, Ok(DecodedReply::Acknowledged));
            }
            h.ack(InstallerRequest::Return {
                projection: 55,
                source: (!source).then_some(peer()),
            });
            h.projection(local(), false);
            h.value["result"]["installer"]["recovery_pending"] = json!(0);
            h.counter(
                if source {
                    "e2_source_returned"
                } else {
                    "e2_dest_returned"
                },
                1,
            );
            h.status();
            if source {
                no_close(&h);
                h.home();
                h.close();
            } else {
                assert!(h.tutorial.detail().local_cleanup_settled);
                assert_eq!(
                    h.tutorial.detail().remote_restoration,
                    RemoteRestoration::Unknown
                );
                assert!(!h.tutorial.detail().cleanup_settled);
                blocked_replacement(&mut h);
                let stale_view = TutorialEvent::User {
                    attempt: h.attempt.attempt,
                    view_revision: 16,
                    action: TutorialUserAction::Confirm(HumanConfirmation::SourceRestored),
                };
                assert_eq!(
                    h.tutorial.reduce(stale_view, h.now),
                    Err(TutorialError::RetiredView)
                );
                h.confirm(HumanConfirmation::SourceRestored);
                assert_eq!(
                    h.tutorial.detail().remote_restoration,
                    RemoteRestoration::HumanConfirmed
                );
            }
            assert!(h.tutorial.detail().cleanup_settled);
            assert_ne!(h.tutorial.state(), TutorialState::Verified);
        }
    }
}
#[test]
fn unknown_play_outcome_uses_owned_snapshot_without_blind_restart_or_close() {
    let mut h = preparation(true);
    let id = play(&mut h);
    h.fixture(Some(id), Err(TutorialFixtureError::TimedOut));
    no_close(&h);
    let observe = h.fixture_action(|a| *a == TutorialFixtureAction::ObserveWindow { fixture: 10 });
    let mut facts = snapshot(None, 0, TutorialWindowFacts::Unknown);
    if let TutorialFixtureObservation::Snapshot { tone, .. } = &mut facts {
        *tone = TutorialToneState::Running { tone: 33 };
    }
    h.fixture(Some(observe), Ok(facts));
    no_close(&h);
    let stop = h.fixture_action(|a| {
        *a == TutorialFixtureAction::StopTone {
            fixture: 10,
            tone: 33,
        }
    });
    h.fixture(
        Some(stop),
        Ok(TutorialFixtureObservation::ToneStopped {
            fixture: 10,
            tone: 33,
        }),
    );
    h.ack(InstallerRequest::Release);
    h.status();
    h.close();
    assert!(h.tutorial.detail().cleanup_settled);
    assert!(
        !h.fixtures
            .iter()
            .any(|(_, a)| matches!(a, TutorialFixtureAction::PlayTone { .. }))
    );
}
#[test]
fn all_four_e2_roles_reject_delayed_pre_authorization_projection_receipts() {
    for role in [
        TutorialRole::E2SourcePush,
        TutorialRole::E2SourcePull,
        TutorialRole::E2DestinationPush,
        TutorialRole::E2DestinationPull,
    ] {
        let mut h = Harness::new(role);
        h.status();
        h.event(TutorialEvent::Tick);
        let status = h.call(&InstallerRequest::Status);
        h.now += 1;
        let at = h.now;
        let source = matches!(
            role,
            TutorialRole::E2SourcePush | TutorialRole::E2SourcePull
        );
        h.projection(if source { local() } else { peer() }, true);
        h.counter(
            if source {
                "e2_source_started"
            } else {
                "e2_dest_started"
            },
            1,
        );
        if source {
            h.value["result"]["installer"]["recovery_pending"] = json!(1);
        }
        let delayed = settings_reply(
            status.id,
            Ok(DecodedReply::Status(StatusAdmission::Supported(health(
                &h.value, h.platform,
            )))),
            at,
        );
        authorize_e2(&mut h);
        h.event(TutorialEvent::Reply(delayed));
        assert_eq!(h.tutorial.state(), TutorialState::Failed);
        assert_eq!(
            h.tutorial.detail().failure,
            Some(TutorialFailure::Isolation)
        );
        assert!(h.retired.contains(&h.attempt.attempt.0));
        assert!(
            !h.calls
                .iter()
                .any(|call| matches!(call.request, InstallerRequest::Return { .. }))
        );
        assert!(!h.tutorial.detail().cleanup_settled);
        blocked_replacement(&mut h);
        h.status(); // fresh polling cannot change ownership of a known pre-action projection
        assert!(
            !h.calls
                .iter()
                .any(|call| matches!(call.request, InstallerRequest::Return { .. }))
        );
        assert_ne!(h.tutorial.state(), TutorialState::Verified);
    }
}
#[test]
fn project_and_pull_ownership_receipts_must_follow_acknowledgement() {
    for role in [TutorialRole::E2SourcePush, TutorialRole::E2DestinationPull] {
        for equal_to_ack in [false, true] {
            let (mut h, command) = pending_projection(role);
            h.event(TutorialEvent::Tick);
            let status = h.call(&InstallerRequest::Status);
            h.now += 1;
            let at = h.now;
            let source = role == TutorialRole::E2SourcePush;
            h.projection(if source { local() } else { peer() }, true);
            h.counter(
                if source {
                    "e2_source_started"
                } else {
                    "e2_dest_started"
                },
                1,
            );
            if source {
                h.value["result"]["installer"]["recovery_pending"] = json!(1);
            }
            let mut delayed = settings_reply(
                status.id,
                Ok(DecodedReply::Status(StatusAdmission::Supported(health(
                    &h.value, h.platform,
                )))),
                at,
            );
            h.reply(command, Ok(DecodedReply::Acknowledged));
            if equal_to_ack {
                delayed.observed_at_ms = h.now;
            }
            h.event(TutorialEvent::Reply(delayed));
            assert_eq!(h.tutorial.state(), TutorialState::Failed);
            assert_eq!(
                h.tutorial.detail().failure,
                Some(TutorialFailure::Isolation)
            );
            assert!(
                !h.calls
                    .iter()
                    .any(|call| matches!(call.request, InstallerRequest::Return { .. }))
            );
            assert_ne!(h.tutorial.state(), TutorialState::Verified);
        }
    }
}
#[test]
fn rejected_recovery_observation_preserves_unknown_play_outcome() {
    let mut h = preparation(true);
    let play = play(&mut h);
    h.fixture(Some(play), Err(TutorialFixtureError::TimedOut));
    let reason = h.tutorial.detail().failure.clone();
    for error in [
        TutorialFixtureError::Busy,
        TutorialFixtureError::Unavailable,
    ] {
        let observe =
            h.fixture_action(|a| *a == TutorialFixtureAction::ObserveWindow { fixture: 10 });
        h.event(TutorialEvent::FixtureSubmitFailed {
            attempt: h.attempt.attempt,
            call_id: observe,
            error,
        });
        no_close(&h);
        blocked_replacement(&mut h);
        assert_eq!(h.tutorial.detail().failure, reason);
        assert!(!h.tutorial.detail().local_cleanup_settled);
        assert!(
            !h.fixtures
                .iter()
                .any(|(_, a)| matches!(a, TutorialFixtureAction::PlayTone { .. }))
        );
        h.status();
        no_close(&h);
    }
    let observe = h.fixture_action(|a| *a == TutorialFixtureAction::ObserveWindow { fixture: 10 });
    let mut facts = snapshot(None, 0, TutorialWindowFacts::Unknown);
    if let TutorialFixtureObservation::Snapshot { tone, .. } = &mut facts {
        *tone = TutorialToneState::Running { tone: 33 };
    }
    h.fixture(Some(observe), Ok(facts));
    no_close(&h);
    let stop = h.fixture_action(|a| {
        *a == TutorialFixtureAction::StopTone {
            fixture: 10,
            tone: 33,
        }
    });
    h.fixture(
        Some(stop),
        Ok(TutorialFixtureObservation::ToneStopped {
            fixture: 10,
            tone: 33,
        }),
    );
    h.ack(InstallerRequest::Release);
    h.status();
    h.close();
    assert!(h.tutorial.detail().cleanup_settled);
    assert_eq!(h.tutorial.detail().failure, reason);
}
#[test]
fn cancelled_destination_failed_return_or_restart_keeps_recovery_uncertain() {
    for restart in [false, true] {
        let mut h = start_e2(TutorialRole::E2DestinationPull);
        cancel(&mut h);
        h.ack(InstallerRequest::Return {
            projection: 55,
            source: Some(peer()),
        });
        h.projection(peer(), false);
        if restart {
            h.value["result"]["installer"]["instance"]["id"] = json!(100);
        } else {
            h.counter("e2_returns_failed", 1);
        }
        for _ in 0..3 {
            h.status();
            assert!(!h.tutorial.detail().cleanup_settled);
        }
        assert_eq!(h.tutorial.state(), TutorialState::Failed);
        assert!(h.tutorial.detail().failure.is_some());
        blocked_replacement(&mut h);
    }
}
#[test]
fn both_destination_roles_separate_local_cleanup_from_correlated_remote_restoration() {
    for role in [
        TutorialRole::E2DestinationPush,
        TutorialRole::E2DestinationPull,
    ] {
        for change in ["none", "instance", "link", "gate"] {
            let mut h = start_e2(role);
            cancel(&mut h);
            h.ack(InstallerRequest::Return {
                projection: 55,
                source: Some(peer()),
            });
            h.ack(InstallerRequest::Release);
            h.projection(peer(), false);
            h.counter("e2_dest_returned", 1);
            h.status();
            assert!(h.tutorial.detail().local_cleanup_settled);
            assert_eq!(
                h.tutorial.detail().remote_restoration,
                RemoteRestoration::Unknown
            );
            assert!(!h.tutorial.detail().cleanup_settled);
            blocked_replacement(&mut h);
            assert_eq!(
                h.tutorial.reduce(
                    TutorialEvent::User {
                        attempt: AttemptId(h.attempt.attempt.0 + 1),
                        view_revision: 17,
                        action: TutorialUserAction::Confirm(HumanConfirmation::SourceRestored),
                    },
                    h.now
                ),
                Err(TutorialError::InvalidAttempt)
            );
            match change {
                "instance" => h.value["result"]["installer"]["instance"]["id"] = json!(100),
                "link" => h.value["result"]["installer"]["peers"][0]["link_generation"] = json!(3),
                "gate" => h.value["result"]["installer"]["epochs"]["gate"] = json!(2),
                _ => {}
            }
            if change == "none" {
                h.confirm(HumanConfirmation::SourceRestored);
                assert!(h.tutorial.detail().cleanup_settled);
                assert_eq!(
                    h.tutorial.detail().remote_restoration,
                    RemoteRestoration::HumanConfirmed
                );
            } else {
                h.status();
                h.now += 1;
                assert_eq!(
                    h.tutorial.reduce(
                        TutorialEvent::User {
                            attempt: h.attempt.attempt,
                            view_revision: 17,
                            action: TutorialUserAction::Confirm(HumanConfirmation::SourceRestored),
                        },
                        h.now
                    ),
                    Err(TutorialError::InvalidObservation)
                );
                assert!(!h.tutorial.detail().cleanup_settled);
                assert_eq!(
                    h.tutorial.detail().remote_restoration,
                    RemoteRestoration::Unknown
                );
            }
            assert_ne!(h.tutorial.state(), TutorialState::Verified);
        }
    }
}
#[test]
fn retired_lost_ack_cleanup_never_adopts_active_projection_after_instance_or_link_change() {
    for role in [TutorialRole::E2SourcePush, TutorialRole::E2DestinationPull] {
        for restart in [false, true] {
            let (mut h, command) = pending_projection(role);
            h.reply(command, Err(CallFailure::TimeoutOutcomeUnknown));
            let reason = h.tutorial.detail().failure.clone();
            h.projection(
                if role == TutorialRole::E2SourcePush {
                    local()
                } else {
                    peer()
                },
                true,
            );
            h.counter(
                if role == TutorialRole::E2SourcePush {
                    "e2_source_started"
                } else {
                    "e2_dest_started"
                },
                1,
            );
            if restart {
                h.value["result"]["installer"]["instance"]["id"] = json!(100);
            } else {
                h.value["result"]["installer"]["peers"][0]["link_generation"] = json!(3);
            }
            for _ in 0..2 {
                h.status();
                assert!(
                    !h.calls
                        .iter()
                        .any(|c| matches!(c.request, InstallerRequest::Return { .. }))
                );
                no_close(&h);
                blocked_replacement(&mut h);
                assert_eq!(h.tutorial.detail().failure, reason);
                assert!(!h.tutorial.detail().local_cleanup_settled);
                if role == TutorialRole::E2DestinationPull {
                    assert_eq!(
                        h.tutorial.detail().remote_restoration,
                        RemoteRestoration::Unknown
                    );
                }
            }
        }
    }
}
#[test]
fn stored_owned_projection_cannot_be_returned_after_instance_or_link_changes() {
    for role in [
        TutorialRole::E2SourcePush,
        TutorialRole::E2SourcePull,
        TutorialRole::E2DestinationPush,
        TutorialRole::E2DestinationPull,
    ] {
        for restart in [false, true] {
            let mut h = start_e2(role);
            if restart {
                h.value["result"]["installer"]["instance"]["id"] = json!(100);
            } else {
                h.value["result"]["installer"]["peers"][0]["link_generation"] = json!(3);
            }
            h.status();
            assert_eq!(h.tutorial.state(), TutorialState::Failed);
            assert_eq!(
                h.tutorial.detail().failure,
                Some(TutorialFailure::BindingChanged)
            );
            for _ in 0..2 {
                assert!(
                    !h.calls
                        .iter()
                        .any(|c| matches!(c.request, InstallerRequest::Return { .. }))
                );
                no_close(&h);
                blocked_replacement(&mut h);
                h.status();
            }
            // Reappearance of the original equality tokens cannot restore retired authority.
            h.value["result"]["installer"]["instance"]["id"] = json!(99);
            h.value["result"]["installer"]["peers"][0]["link_generation"] = json!(2);
            h.status();
            assert!(
                !h.calls
                    .iter()
                    .any(|c| matches!(c.request, InstallerRequest::Return { .. }))
            );
            no_close(&h);
        }
    }
}
#[test]
fn incomplete_health_holds_owned_cleanup_until_original_correlation_is_observed() {
    let mut h = start_e2(TutorialRole::E2SourcePush);
    h.event(TutorialEvent::Tick);
    let call = h.call(&InstallerRequest::Status);
    let mut incomplete = h.value.clone();
    incomplete["result"]["installer"]
        .as_object_mut()
        .unwrap()
        .remove("instance");
    let reply = parse_status(&serde_json::to_vec(&incomplete).unwrap(), h.platform).unwrap();
    h.reply(call, Ok(DecodedReply::Status(reply)));
    assert_eq!(h.tutorial.state(), TutorialState::PendingContract);
    assert!(
        !h.calls
            .iter()
            .any(|c| matches!(c.request, InstallerRequest::Return { .. }))
    );
    no_close(&h);
    blocked_replacement(&mut h);
    h.status();
    assert!(h.calls.iter().any(|c| c.request
        == InstallerRequest::Return {
            projection: 55,
            source: None
        }));
    no_close(&h);
    assert_ne!(h.tutorial.state(), TutorialState::Verified);
}
#[test]
fn failed_source_restoration_reason_persists_across_later_detection_and_human_ticks() {
    let mut h = start_e2(TutorialRole::E2SourcePull);
    cancel(&mut h);
    h.ack(InstallerRequest::Return {
        projection: 55,
        source: None,
    });
    h.projection(local(), false);
    h.value["result"]["installer"]["recovery_pending"] = json!(0);
    h.counter("e2_returns_failed", 1);
    h.status();
    let reason = h.tutorial.detail().failure.clone();
    h.counter("e2_source_returned", 1);
    for _ in 0..3 {
        h.status();
        no_close(&h);
        assert_eq!(h.tutorial.detail().failure, reason);
    }
    blocked_replacement(&mut h);
}
#[test]
fn running_owned_audio_cancel_stops_only_its_tone_then_closes_only_its_child() {
    let mut h = audio_start(true);
    cancel(&mut h);
    no_close(&h);
    let stop = h.fixture_action(|a| {
        *a == TutorialFixtureAction::StopTone {
            fixture: 10,
            tone: 33,
        }
    });
    h.fixture(
        Some(stop),
        Ok(TutorialFixtureObservation::ToneStopped {
            fixture: 10,
            tone: 33,
        }),
    );
    h.ack(InstallerRequest::Release);
    h.value["result"]["installer"]["audio"]["active_peers"] = json!([]);
    h.status();
    h.close();
    assert!(h.tutorial.detail().cleanup_settled);
    assert_ne!(h.tutorial.state(), TutorialState::Verified);
}
#[test]
fn both_audio_roles_reject_missing_hearing_opposite_wrong_and_intervening_peers() {
    for sender in [false, true] {
        for missing in ["hearing", "opposite", "wrong", "between"] {
            let mut h = audio_start(sender);
            if missing != "hearing" {
                h.confirm(if sender {
                    HumanConfirmation::FarSpeakerHeard
                } else {
                    HumanConfirmation::LocalSpeakerHeard
                });
            }
            h.confirm(HumanConfirmation::ExclusiveAudioInterval);
            match missing {
                "opposite" => {
                    h.value["result"]["installer"]["audio"][if sender {
                        "frames_played"
                    } else {
                        "frames_sent"
                    }] = json!(1)
                }
                "wrong" => {
                    h.value["result"]["installer"]["audio"]["active_peers"] =
                        json!([NodeId([0x33; 32])])
                }
                "between" => {
                    h.value["result"]["installer"]["audio"]["active_peers"] =
                        json!([peer(), NodeId([0x33; 32])])
                }
                _ => {}
            }
            h.value["result"]["installer"]["audio"][if sender {
                "frames_sent"
            } else {
                "frames_played"
            }] = json!(if missing == "between" { 0 } else { 10 });
            h.status();
            if missing != "hearing" {
                assert_eq!(h.tutorial.state(), TutorialState::Failed);
                assert!(h.retired.contains(&h.attempt.attempt.0));
                if missing == "opposite" {
                    assert!(matches!(
                        h.tutorial.detail().failure,
                        Some(TutorialFailure::Evidence(_))
                    ));
                } else {
                    assert_eq!(
                        h.tutorial.detail().failure,
                        Some(TutorialFailure::Isolation)
                    );
                }
                h.value["result"]["installer"]["audio"]["active_peers"] = json!([peer()]);
                h.value["result"]["installer"]["audio"][if sender {
                    "frames_sent"
                } else {
                    "frames_played"
                }] = json!(10);
                h.status();
            }
            if sender {
                let stop = h.fixture_action(|a| {
                    *a == TutorialFixtureAction::StopTone {
                        fixture: 10,
                        tone: 33,
                    }
                });
                h.fixture(
                    Some(stop),
                    Ok(TutorialFixtureObservation::ToneStopped {
                        fixture: 10,
                        tone: 33,
                    }),
                );
                if missing == "hearing" {
                    assert!(
                        !h.fixtures
                            .iter()
                            .any(|(_, a)| matches!(a, TutorialFixtureAction::Close { .. }))
                    );
                } else {
                    h.close();
                }
            }
            if missing == "hearing" {
                assert_eq!(h.tutorial.state(), TutorialState::WaitingUser);
                h.confirm(if sender {
                    HumanConfirmation::FarSpeakerHeard
                } else {
                    HumanConfirmation::LocalSpeakerHeard
                });
                if sender {
                    h.close();
                }
                h.verified();
            } else {
                assert_ne!(h.tutorial.state(), TutorialState::Verified);
            }
            assert!(
                h.tutorial.detail().global_audio_counters
                    && h.tutorial.detail().sampled_peer_membership
                    && h.tutorial.detail().human_hearing_required
                    && h.tutorial.detail().unseen_between_poll_audio_possible
            );
        }
    }
}

#[test]
fn stale_agent_and_fixture_receipts_and_completed_attempt_replies_leave_evidence_unchanged() {
    let mut h = Harness::new(TutorialRole::E1Controller);
    h.status();
    let old_time = h.now;
    h.event(TutorialEvent::Tick);
    let call = h.call(&InstallerRequest::Status);
    let before = h.tutorial.current_samples().to_vec();
    let stale = settings_reply(
        call.id,
        Ok(DecodedReply::Status(StatusAdmission::Supported(health(
            &h.value, h.platform,
        )))),
        old_time - 1,
    );
    assert_eq!(
        h.tutorial.reduce(TutorialEvent::Reply(stale), h.now),
        Err(TutorialError::InvalidObservation)
    );
    assert_eq!(h.tutorial.current_samples(), before);
    h.counter("e1_controller_started", 1);
    h.counter("e1_controller_ended", 1);
    h.counter("e1_chord_releases", 1);
    h.now += 1;
    let receipt = h.now;
    let accepted = settings_reply(
        call.id,
        Ok(DecodedReply::Status(StatusAdmission::Supported(health(
            &h.value, h.platform,
        )))),
        receipt,
    );
    h.now += 20;
    h.event(TutorialEvent::Reply(accepted.clone()));
    assert!(
        h.tutorial
            .current_samples()
            .iter()
            .all(|s| s.observed_at_ms == receipt && s.source == ObservationSource::Live)
    );
    h.confirm(HumanConfirmation::RemotePracticeAndHud);
    h.verified();
    assert!(h.observed.iter().any(|e| matches!(&e.kind, TutorialEffectKind::Core(FlowEvent::Verified { verification, .. }) if verification.observed_at_ms == receipt && verification.source == ObservationSource::Live)));
    h.next(TutorialRole::E1Target, Some(peer()));
    h.status();
    h.open();
    let before = h.tutorial.current_samples().to_vec();
    assert_eq!(
        h.tutorial.reduce(TutorialEvent::Reply(accepted), h.now),
        Err(TutorialError::InvalidObservation)
    );
    let stale_fixture = TutorialEvent::Fixture {
        attempt: h.attempt.attempt,
        call_id: None,
        sequence: h.sequence + 1,
        observed_at_ms: 1,
        result: Ok(snapshot(Some(2), 1, TutorialWindowFacts::Unknown)),
    };
    assert_eq!(
        h.tutorial.reduce(stale_fixture, h.now),
        Err(TutorialError::InvalidObservation)
    );
    let retired_fixture = TutorialEvent::Fixture {
        attempt: AttemptId(1),
        call_id: None,
        sequence: 999,
        observed_at_ms: h.now,
        result: Ok(snapshot(Some(1), 1, TutorialWindowFacts::Unknown)),
    };
    assert_eq!(
        h.tutorial.reduce(retired_fixture, h.now),
        Err(TutorialError::InvalidAttempt)
    );
    assert_eq!(h.tutorial.current_samples(), before);
    let arm = h.fixture_action(|a| matches!(a, TutorialFixtureAction::ArmTarget { .. }));
    h.fixture(
        Some(arm),
        Ok(TutorialFixtureObservation::TargetArmed {
            fixture: 10,
            phase: 2,
        }),
    );
    let last = h.now;
    h.now += 10;
    h.sequence += 1;
    h.event(TutorialEvent::Fixture {
        attempt: h.attempt.attempt,
        call_id: None,
        sequence: h.sequence,
        observed_at_ms: last + 1,
        result: Ok(snapshot(Some(2), 1, TutorialWindowFacts::Unknown)),
    });
    h.sequence += 1;
    assert_eq!(
        h.tutorial.reduce(
            TutorialEvent::Fixture {
                attempt: h.attempt.attempt,
                call_id: None,
                sequence: h.sequence,
                observed_at_ms: last,
                result: Ok(snapshot(Some(2), 2, TutorialWindowFacts::Unknown))
            },
            h.now
        ),
        Err(TutorialError::InvalidObservation)
    );
}
#[test]
fn settings_plan_replacement_retires_old_view_and_requires_new_consent() {
    let mut h = Harness::new(TutorialRole::E1Controller);
    let mut s = settings_ready(&mut h);
    let mut v = h.value.clone();
    v["result"]["installer"]["instance"]["id"] = json!(100);
    v["result"]["installer"]["config_revision"] = json!("2222222222222222");
    h.now += 1;
    let observed = s
        .detected(
            &health(&v, AgentPlatform::Linux),
            ObservationSource::Live,
            h.now,
            h.now,
            17,
        )
        .unwrap();
    reduce_settings(&mut h.core, &observed, h.now);
    assert_eq!(s.state(), &SettingsTransitionState::NeedsDetection);
    assert_eq!(s.last_error(), Some(&CallFailure::InvalidResponse));
    assert_eq!(
        h.core
            .summary(h.now, s.current_samples())
            .steps
            .iter()
            .find(|step| step.id == h.attempt.step)
            .unwrap()
            .state,
        StepState::Stale
    );
    assert!(s.consent_update(1, 17, true).is_err());
    h.now += 1;
    s.detected(
        &health(&v, AgentPlatform::Linux),
        ObservationSource::Live,
        h.now,
        h.now,
        18,
    )
    .unwrap();
    assert!(s.consent_update(1, 17, true).is_err());
    assert_eq!(
        s.consent_update(1, 18, true).unwrap().request,
        InstallerRequest::SettingsUpdate {
            expected_revision: "2222222222222222".into(),
            mac_virtual_display: true
        }
    );
}
fn reduce_settings(core: &mut Flow, events: &[FlowEvent], now: u64) {
    assert!(events.is_empty() || matches!(events[0], FlowEvent::Observe { .. }));
    for event in events {
        core.reduce(event.clone(), now).unwrap();
    }
}
fn settings_ready(h: &mut Harness) -> SettingsTransition {
    h.status();
    controller_end(h, true);
    h.confirm(HumanConfirmation::RemotePracticeAndHud);
    h.verified();
    let mut s = SettingsTransition::new(local(), 17);
    s.track_peers(&[peer()]).unwrap();
    h.now += 1;
    let events = s
        .detected(
            &health(&h.value, h.platform),
            ObservationSource::Live,
            h.now,
            h.now,
            17,
        )
        .unwrap();
    reduce_settings(&mut h.core, &events, h.now);
    s
}
fn settings_updated(s: &mut SettingsTransition, h: &mut Harness) {
    s.consent_update(100, s.view_revision(), true).unwrap();
    h.now += 1;
    let o = s
        .reply(
            settings_reply(
                100,
                Ok(DecodedReply::SettingsUpdated(SettingsUpdated {
                    revision: "2222222222222222".into(),
                    restart_required: true,
                })),
                h.now,
            ),
            h.now,
        )
        .unwrap();
    reduce_settings(&mut h.core, &o.observations, h.now);
}
fn restart_settings(s: &mut SettingsTransition, h: &mut Harness, id: u64) {
    let samples = s.current_samples().to_vec();
    let (events, call) = s
        .consent_restart(id, s.view_revision(), &[h.attempt.step], &samples)
        .unwrap();
    let FlowEvent::Observe { samples } = &events[0] else {
        panic!()
    };
    assert_eq!(samples.len(), 2);
    assert_eq!(call.request, InstallerRequest::Restart);
    reduce_settings(&mut h.core, &events, h.now);
    assert_ne!(
        h.core
            .summary(h.now, samples)
            .steps
            .iter()
            .find(|step| step.id == h.attempt.step)
            .unwrap()
            .state,
        StepState::Satisfied
    );
}
#[test]
fn settings_conflict_recovery_restart_loads_disk_before_renewed_update_consent() {
    let mut h = Harness::new(TutorialRole::E1Controller);
    let mut s = settings_ready(&mut h);
    s.consent_update(100, 17, true).unwrap();
    h.now += 1;
    let outcome = s
        .reply(
            settings_reply(
                100,
                Err(CallFailure::Refused(AgentRefusal::RevisionConflict)),
                h.now,
            ),
            h.now,
        )
        .unwrap();
    assert!(outcome.detect_after_unknown);
    reduce_settings(&mut h.core, &outcome.observations, h.now);
    h.now += 1;
    let detected = s
        .detected(
            &health(&h.value, h.platform),
            ObservationSource::Live,
            h.now,
            h.now,
            18,
        )
        .unwrap();
    reduce_settings(&mut h.core, &detected, h.now);
    let FlowEvent::Observe { samples } = &detected[0] else {
        panic!()
    };
    assert_eq!(
        s.state(),
        &SettingsTransitionState::NeedsRecoveryRestartConsent
    );
    assert!(s.consent_update(101, 18, true).is_err());
    let (events, call) = s
        .consent_restart(101, 18, &[h.attempt.step], samples)
        .unwrap();
    reduce_settings(&mut h.core, &events, h.now);
    assert_eq!(call.request, InstallerRequest::Restart);
    h.now += 1;
    s.reply(
        settings_reply(101, Ok(DecodedReply::Acknowledged), h.now),
        h.now,
    )
    .unwrap();
    assert_ne!(s.state(), &SettingsTransitionState::Complete);
    s.poll_new_instance(102).unwrap();
    h.now += 1;
    h.value["result"]["installer"]["instance"]["id"] = json!(100);
    h.value["result"]["installer"]["config_revision"] = json!("3333333333333333");
    let o = s
        .reply(
            settings_reply(
                102,
                Ok(DecodedReply::Status(StatusAdmission::Supported(health(
                    &h.value, h.platform,
                )))),
                h.now,
            ),
            h.now,
        )
        .unwrap();
    reduce_settings(&mut h.core, &o.observations, h.now);
    assert_eq!(s.state(), &SettingsTransitionState::NeedsDetection);
    assert!(s.consent_update(103, 18, true).is_err());
    let view = s.view_revision();
    h.now += 1;
    reduce_settings(
        &mut h.core,
        &s.detected(
            &health(&h.value, h.platform),
            ObservationSource::Live,
            h.now,
            h.now,
            view,
        )
        .unwrap(),
        h.now,
    );
    assert_eq!(
        s.consent_update(103, view, true).unwrap().request,
        InstallerRequest::SettingsUpdate {
            expected_revision: "3333333333333333".into(),
            mac_virtual_display: true
        }
    );
}
#[test]
fn restart_timeout_refusal_and_transient_poll_error_keep_revision_and_require_detection() {
    for error in [
        CallFailure::TimeoutOutcomeUnknown,
        CallFailure::Refused(AgentRefusal::NotSupported),
    ] {
        let mut h = Harness::new(TutorialRole::E1Controller);
        let mut s = settings_ready(&mut h);
        settings_updated(&mut s, &mut h);
        restart_settings(&mut s, &mut h, 101);
        h.now += 1;
        let o = s
            .reply(settings_reply(101, Err(error.clone()), h.now), h.now)
            .unwrap();
        reduce_settings(&mut h.core, &o.observations, h.now);
        assert!(o.detect_after_unknown);
        assert_eq!(s.last_error(), Some(&error));
        assert_eq!(s.returned_revision(), Some("2222222222222222"));
        assert!(s.consent_restart(102, 17, &[], &[]).is_err());
        s.poll_new_instance(102).unwrap();
        h.now += 1;
        let o = s
            .reply(
                settings_reply(102, Err(CallFailure::Unavailable), h.now),
                h.now,
            )
            .unwrap();
        assert!(o.detect_after_unknown);
        assert_eq!(s.state(), &SettingsTransitionState::WaitingNewInstance);
        s.poll_new_instance(103).unwrap();
        h.now += 1;
        let o = s
            .reply(
                settings_reply(
                    103,
                    Ok(DecodedReply::Status(StatusAdmission::Supported(health(
                        &h.value, h.platform,
                    )))),
                    h.now,
                ),
                h.now,
            )
            .unwrap();
        reduce_settings(&mut h.core, &o.observations, h.now);
        assert_eq!(s.state(), &SettingsTransitionState::NeedsRestartConsent);
        assert!(s.consent_restart(104, 17, &[h.attempt.step], &[]).is_err());
        let FlowEvent::Observe { samples } = &o.observations[0] else {
            panic!()
        };
        let (events, _) = s
            .consent_restart(104, s.view_revision(), &[h.attempt.step], samples)
            .unwrap();
        reduce_settings(&mut h.core, &events, h.now);
        h.now += 1;
        s.reply(
            settings_reply(104, Ok(DecodedReply::Acknowledged), h.now),
            h.now,
        )
        .unwrap();
        s.poll_new_instance(105).unwrap();
        h.now += 1;
        h.value["result"]["installer"]["instance"]["id"] = json!(100);
        h.value["result"]["installer"]["config_revision"] = json!("2222222222222222");
        let o = s
            .reply(
                settings_reply(
                    105,
                    Ok(DecodedReply::Status(StatusAdmission::Supported(health(
                        &h.value, h.platform,
                    )))),
                    h.now,
                ),
                h.now,
            )
            .unwrap();
        reduce_settings(&mut h.core, &o.observations, h.now);
        assert_eq!(s.state(), &SettingsTransitionState::Complete);
    }
}
#[test]
fn settings_new_instance_wrong_node_and_incomplete_status_cannot_complete() {
    let mut h = Harness::new(TutorialRole::E1Controller);
    let mut s = settings_ready(&mut h);
    settings_updated(&mut s, &mut h);
    restart_settings(&mut s, &mut h, 101);
    h.now += 1;
    s.reply(
        settings_reply(101, Ok(DecodedReply::Acknowledged), h.now),
        h.now,
    )
    .unwrap();
    s.poll_new_instance(102).unwrap();
    h.now += 1;
    let mut foreign = h.value.clone();
    foreign["result"]["installer"]["node"] = json!("33".repeat(32));
    assert!(
        s.reply(
            settings_reply(
                102,
                Ok(DecodedReply::Status(StatusAdmission::Supported(health(
                    &foreign, h.platform
                )))),
                h.now
            ),
            h.now
        )
        .is_err()
    );
    let o = s
        .reply(
            settings_reply(
                102,
                Ok(DecodedReply::Status(
                    StatusAdmission::PendingHealthContract(PendingHealthReason::Incomplete),
                )),
                h.now,
            ),
            h.now,
        )
        .unwrap();
    reduce_settings(&mut h.core, &o.observations, h.now);
    assert_eq!(o.observations, vec![FlowEvent::Observe { samples: vec![] }]);
    assert_eq!(s.state(), &SettingsTransitionState::WaitingNewInstance);
    s.poll_new_instance(103).unwrap();
    h.now += 1;
    h.value["result"]["installer"]["instance"]["id"] = json!(100);
    h.value["result"]["installer"]["config_revision"] = json!("2222222222222222");
    let o = s
        .reply(
            settings_reply(
                103,
                Ok(DecodedReply::Status(StatusAdmission::Supported(health(
                    &h.value, h.platform,
                )))),
                h.now,
            ),
            h.now,
        )
        .unwrap();
    reduce_settings(&mut h.core, &o.observations, h.now);
    assert_eq!(s.state(), &SettingsTransitionState::Complete);
}

#[test]
fn all_four_e2_roles_on_one_tutorial_cannot_borrow_counters_or_confirmations() {
    let mut h = start_e2(TutorialRole::E2SourcePush);
    end_e2(&mut h);
    h.verified();
    for role in [
        TutorialRole::E2SourcePull,
        TutorialRole::E2DestinationPush,
        TutorialRole::E2DestinationPull,
    ] {
        h.next(role, Some(peer()));
        h.status();
        assert_ne!(h.tutorial.state(), TutorialState::Verified);
        authorize_e2(&mut h);
        observe_e2(&mut h);
        let source = role == TutorialRole::E2SourcePull;
        h.projection(local(), false);
        h.value["result"]["installer"]["recovery_pending"] = json!(0);
        let metric = if source {
            "e2_source_returned"
        } else {
            "e2_dest_returned"
        };
        let returned = h.value["result"]["installer"]["peers"][0]["counters"][metric]
            .as_u64()
            .unwrap();
        h.counter(metric, returned + 1);
        if !source {
            let frames =
                h.value["result"]["installer"]["peers"][0]["counters"]["e2_frames_presented"]
                    .as_u64()
                    .unwrap();
            h.counter("e2_frames_presented", frames + 10);
        }
        h.status();
        if source {
            h.home();
        } else {
            h.confirm(HumanConfirmation::SourceRestored);
        }
        assert_ne!(
            h.tutorial.state(),
            TutorialState::Verified,
            "prior destination human confirmation cannot carry over"
        );
        h.confirm(HumanConfirmation::DestinationPatternInteractionAndClose);
        if source {
            h.close();
        }
        h.verified();
    }
    assert_eq!(h.attempt.attempt, AttemptId(4));
    let calls: Vec<_> = h
        .observed
        .iter()
        .filter_map(|e| match &e.kind {
            TutorialEffectKind::Agent(c) => Some(c.id),
            TutorialEffectKind::Fixture { call_id, .. } => Some(*call_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        calls.len(),
        calls
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    );
    assert!(calls.windows(2).all(|ids| ids[0] < ids[1]));
}
#[test]
fn idle_exhaustion_and_duplicate_or_decreasing_attempt_and_operation_are_explicit() {
    let mut h = Harness::new(TutorialRole::E1Controller);
    h.status();
    controller_end(&mut h, true);
    h.confirm(HumanConfirmation::RemotePracticeAndHud);
    h.verified();
    for (attempt, operation, error) in [
        (1, 100, TutorialError::InvalidAttempt),
        (0, 100, TutorialError::InvalidAttempt),
        (2, h.attempt.operation.0, TutorialError::WrongOperation),
        (2, 0, TutorialError::WrongOperation),
        (u64::MAX, 100, TutorialError::IdExhausted),
        (2, u64::MAX, TutorialError::IdExhausted),
    ] {
        let mut a = h.attempt.clone();
        a.attempt = AttemptId(attempt);
        a.operation = OperationId(operation);
        assert_eq!(
            h.tutorial.begin(
                a.clone(),
                context(),
                JobIntent {
                    step: a.step,
                    operation: a.operation,
                    stage: JobStage::Verify
                },
                17,
                h.now
            ),
            Err(error)
        );
    }
    let mut t = Tutorial::new();
    t.advance_call_ids(u64::MAX - 1).unwrap();
    let a = h.attempt.clone();
    assert_eq!(
        t.begin(
            a.clone(),
            context(),
            JobIntent {
                step: a.step,
                operation: a.operation,
                stage: JobStage::Verify
            },
            17,
            h.now
        ),
        Err(TutorialError::IdExhausted)
    );
    let mut s = SettingsTransition::new(local(), 17);
    s.detected(
        &health(&value(), AgentPlatform::Linux),
        ObservationSource::Live,
        1,
        1,
        17,
    )
    .unwrap();
    assert_eq!(s.state(), &SettingsTransitionState::NeedsConsent);
    assert!(s.consent_update(u64::MAX, 17, true).is_err());
    assert_eq!(s.last_call_id(), 0);
}
#[test]
fn neither_audio_role_can_replace_exclusive_interval_with_hearing_alone() {
    for sender in [false, true] {
        let mut h = audio_start(sender);
        h.value["result"]["installer"]["audio"][if sender {
            "frames_sent"
        } else {
            "frames_played"
        }] = json!(10);
        h.status();
        if sender {
            let id = h.fixture_action(|a| matches!(a, TutorialFixtureAction::StopTone { .. }));
            h.fixture(
                Some(id),
                Ok(TutorialFixtureObservation::ToneStopped {
                    fixture: 10,
                    tone: 33,
                }),
            );
        }
        h.confirm(if sender {
            HumanConfirmation::FarSpeakerHeard
        } else {
            HumanConfirmation::LocalSpeakerHeard
        });
        assert_ne!(h.tutorial.state(), TutorialState::Verified);
        h.confirm(HumanConfirmation::ExclusiveAudioInterval);
        if sender {
            h.close();
        }
        h.verified();
    }
}

#[test]
fn pending_play_binding_change_preserves_late_tone_cleanup_authority() {
    let mut h = preparation(true);
    let id = play(&mut h);
    h.value["result"]["installer"]["epochs"]["gate"] = json!(2);
    h.status();
    assert_eq!(h.tutorial.state(), TutorialState::Failed);
    no_close(&h);
    tone_started(&mut h, id);
    assert!(h.fixtures.iter().any(|(_, a)| *a
        == TutorialFixtureAction::StopTone {
            fixture: 10,
            tone: 33
        }));
    no_close(&h);
    blocked_replacement(&mut h);
}
#[test]
fn restart_timeout_can_complete_from_detected_new_instance_without_another_mutation() {
    let mut h = Harness::new(TutorialRole::E1Controller);
    let mut s = settings_ready(&mut h);
    settings_updated(&mut s, &mut h);
    restart_settings(&mut s, &mut h, 101);
    h.now += 1;
    let o = s
        .reply(
            settings_reply(101, Err(CallFailure::TimeoutOutcomeUnknown), h.now),
            h.now,
        )
        .unwrap();
    assert!(o.detect_after_unknown);
    assert_eq!(s.returned_revision(), Some("2222222222222222"));
    s.poll_new_instance(102).unwrap();
    h.now += 1;
    h.value["result"]["installer"]["instance"]["id"] = json!(100);
    h.value["result"]["installer"]["config_revision"] = json!("2222222222222222");
    let o = s
        .reply(
            settings_reply(
                102,
                Ok(DecodedReply::Status(StatusAdmission::Supported(health(
                    &h.value, h.platform,
                )))),
                h.now,
            ),
            h.now,
        )
        .unwrap();
    reduce_settings(&mut h.core, &o.observations, h.now);
    assert_eq!(s.state(), &SettingsTransitionState::Complete);
    assert_eq!(s.last_call_id(), 102);
}

#[test]
fn fixture_receipt_before_its_issued_call_cannot_advance_audio_preparation() {
    let mut h = preparation(true);
    let before_call = h.now;
    let id = play(&mut h);
    let before = h.tutorial.current_samples().to_vec();
    assert_eq!(
        h.tutorial.reduce(
            TutorialEvent::Fixture {
                attempt: h.attempt.attempt,
                call_id: Some(id),
                sequence: h.sequence + 1,
                observed_at_ms: before_call,
                result: Ok(TutorialFixtureObservation::ToneStarted {
                    fixture: 10,
                    tone: 33
                })
            },
            h.now
        ),
        Err(TutorialError::InvalidObservation)
    );
    assert_eq!(h.tutorial.current_samples(), before);
    tone_started(&mut h, id);
    h.value["result"]["installer"]["audio"]["active_peers"] = json!([peer()]);
    h.status();
    audio_end(&mut h, true);
    h.verified();
}

#[test]
fn recovery_restart_consent_is_also_retired_by_same_view_plan_replacement() {
    let mut s = SettingsTransition::new(local(), 17);
    let mut v = value();
    s.detected(
        &health(&v, AgentPlatform::Linux),
        ObservationSource::Live,
        1,
        1,
        17,
    )
    .unwrap();
    s.consent_update(1, 17, true).unwrap();
    s.reply(
        settings_reply(
            1,
            Err(CallFailure::Refused(AgentRefusal::RevisionConflict)),
            2,
        ),
        2,
    )
    .unwrap();
    s.detected(
        &health(&v, AgentPlatform::Linux),
        ObservationSource::Live,
        3,
        3,
        18,
    )
    .unwrap();
    assert_eq!(
        s.state(),
        &SettingsTransitionState::NeedsRecoveryRestartConsent
    );
    v["result"]["installer"]["instance"]["id"] = json!(100);
    v["result"]["installer"]["config_revision"] = json!("3333333333333333");
    let observed = s
        .detected(
            &health(&v, AgentPlatform::Linux),
            ObservationSource::Live,
            4,
            4,
            18,
        )
        .unwrap();
    assert!(
        matches!(&observed[0], FlowEvent::Observe { samples } if samples.iter().all(|sample| sample.binding.epochs.instance_id == 100))
    );
    assert_eq!(s.state(), &SettingsTransitionState::NeedsDetection);
    assert_eq!(s.last_error(), Some(&CallFailure::InvalidResponse));
    let samples = s.current_samples().to_vec();
    assert!(s.consent_restart(2, 18, &[], &samples).is_err());
    assert!(s.consent_update(2, 18, true).is_err());
}
