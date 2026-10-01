//! WP-3.5: the macOS CoreAudio host, driven against a fake HAL. No test calls a live CoreAudio
//! function, opens a microphone, triggers a permission prompt, or changes a default device; the only
//! real-HAL object ever created is `CoreAudioHost::new`, which makes no HAL call.
#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "audio/fake_hal.rs"]
mod fake_hal;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crosspane_platform::{
    AudioDeviceError, AudioEvent, AudioFormat, AudioHost, AudioKind, EventSink, IoGate,
    PlatformError, VirtualPorts,
};
use crosspane_platform_macos::audio::hal::{
    DeviceId, DeviceInfo, HalError, IoBuffer, IoCallback, ListenTarget, StreamFormat, StreamId,
};
use crosspane_platform_macos::audio::{
    CoreAudioHost, MIC_APP_UID, MIC_LOOPBACK_UID, PEER_BUSY, SPEAKERS_APP_UID,
    SPEAKERS_LOOPBACK_UID,
};
use crosspane_types::id::NodeId;
use fake_hal::{
    BUILTIN, Buf, Call, FakeHal, MIC_APP, MIC_LOOP, OTHER_OUTPUT, SPEAKERS_APP, SPEAKERS_LOOP,
    TRANSPORT_BUILTIN, contract_streams, run_cycle, run_cycle_raw, stereo_pattern,
};
use rtrb::Consumer;

// -- helpers ----------------------------------------------------------------------------------

fn peer(n: u8) -> NodeId {
    NodeId([n; 32])
}

#[derive(Default)]
struct Events {
    events: Mutex<Vec<AudioEvent>>,
    cv: Condvar,
}

impl EventSink<AudioEvent> for Events {
    fn send(&self, event: AudioEvent) {
        self.events.lock().unwrap().push(event);
        self.cv.notify_all();
    }
}

impl Events {
    fn take(&self) -> Vec<AudioEvent> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }

    fn wait_for(&self, count: usize) -> Vec<AudioEvent> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut events = self.events.lock().unwrap();
        while events.len() < count && Instant::now() < deadline {
            events = self
                .cv
                .wait_timeout(events, Duration::from_millis(20))
                .unwrap()
                .0;
        }
        std::mem::take(&mut *events)
    }
}

fn active(peer: NodeId, kind: AudioKind, active: bool) -> AudioEvent {
    AudioEvent::VirtualActive { peer, kind, active }
}

fn device_error(peer: Option<NodeId>, kind: AudioKind, error: AudioDeviceError) -> AudioEvent {
    AudioEvent::DeviceError { peer, kind, error }
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn open_gate() -> Arc<IoGate> {
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    gate
}

struct Rig {
    hal: Arc<FakeHal>,
    gate: Arc<IoGate>,
    host: CoreAudioHost,
    events: Arc<Events>,
}

impl Rig {
    fn new() -> Self {
        let hal = FakeHal::new();
        let gate = open_gate();
        let host = CoreAudioHost::with_hal(gate.clone(), hal.clone()).unwrap();
        Self {
            hal,
            gate,
            host,
            events: Arc::new(Events::default()),
        }
    }

    fn subscribed() -> Self {
        let mut rig = Self::new();
        rig.host.subscribe(rig.events.clone()).unwrap();
        rig
    }

    fn sync(&self) {
        self.host.sync().unwrap();
    }

    /// A subscribed rig with peer 1 bound and no demand.
    fn bound() -> (Self, VirtualPorts) {
        let mut rig = Self::subscribed();
        let ports = rig.host.add_peer(peer(1), "mac").unwrap();
        assert!(rig.events.take().is_empty());
        (rig, ports)
    }

    /// Peer 1 bound, the speakers running, forwarding started. Events are cleared.
    fn forwarding() -> (Self, VirtualPorts, Arc<dyn IoCallback>) {
        let (rig, ports) = Self::bound();
        rig.hal.app_start(SPEAKERS_APP);
        rig.sync();
        let callback = rig.hal.callback(SPEAKERS_LOOP).expect("forwarding started");
        assert_eq!(
            rig.events.take(),
            vec![active(peer(1), AudioKind::Speaker, true)]
        );
        (rig, ports, callback)
    }
}

fn drain(consumer: &mut Consumer<f32>) -> Vec<f32> {
    let mut out = Vec::new();
    while let Ok(sample) = consumer.pop() {
        out.push(sample);
    }
    out
}

fn forward(callback: &dyn IoCallback, samples: &[f32]) {
    run_cycle(callback, &mut [Buf::new(2, samples)], &mut []);
}

fn is_unsupported(result: Result<impl Sized, PlatformError>) -> bool {
    matches!(result, Err(PlatformError::Unsupported(_)))
}

// -- inventory --------------------------------------------------------------------------------

#[test]
fn the_four_uids_are_the_frozen_literals() {
    assert_eq!(
        SPEAKERS_APP_UID,
        "io.frostdev.crosspane.audio.v0.speakers.app"
    );
    assert_eq!(
        SPEAKERS_LOOPBACK_UID,
        "io.frostdev.crosspane.audio.v0.speakers.loopback"
    );
    assert_eq!(MIC_APP_UID, "io.frostdev.crosspane.audio.v0.microphone.app");
    assert_eq!(
        MIC_LOOPBACK_UID,
        "io.frostdev.crosspane.audio.v0.microphone.loopback"
    );
}

#[test]
fn a_conforming_inventory_binds() {
    let mut rig = Rig::new();
    assert!(rig.host.add_peer(peer(1), "mac").is_ok());
}

#[test]
fn every_deviation_from_the_contract_is_refused_before_any_listener_or_io() {
    type Edit = Box<dyn Fn(&mut DeviceInfo)>;
    let (sa, sl, ma, ml) = (SPEAKERS_APP, SPEAKERS_LOOP, MIC_APP, MIC_LOOP);
    let cases: Vec<(&str, DeviceId, Edit)> = vec![
        (
            "read-back uid",
            sl,
            Box::new(|i| i.uid = "io.frostdev.crosspane.other".into()),
        ),
        ("class", sa, Box::new(|i| i.class_id = 0x1234)),
        (
            "transport",
            sa,
            Box::new(|i| i.transport = TRANSPORT_BUILTIN),
        ),
        ("not alive", ml, Box::new(|i| i.alive = false)),
        ("visible but hidden", sa, Box::new(|i| i.hidden = true)),
        ("hidden but visible", sl, Box::new(|i| i.hidden = false)),
        (
            "wrong direction",
            sa,
            Box::new(|i| {
                i.input_streams = std::mem::take(&mut i.output_streams);
            }),
        ),
        (
            "extra opposite stream",
            sa,
            Box::new(|i| i.input_streams = contract_streams(2, 9001)),
        ),
        ("no stream", ma, Box::new(|i| i.input_streams.clear())),
        (
            "two streams",
            sl,
            Box::new(|i| {
                let extra = i.input_streams[0];
                i.input_streams.push(extra);
            }),
        ),
        (
            "rate 44.1k",
            sa,
            Box::new(|i| i.output_streams[0].format.sample_rate = 44_100.0),
        ),
        (
            "non-interleaved",
            sa,
            Box::new(|i| i.output_streams[0].format = StreamFormat::float32(2, false)),
        ),
        (
            "integer flags",
            sa,
            Box::new(|i| i.output_streams[0].format.format_flags = 12),
        ),
        (
            "mono speakers",
            sa,
            Box::new(|i| i.output_streams[0].format = StreamFormat::float32(1, true)),
        ),
        (
            "stereo microphone",
            ma,
            Box::new(|i| i.input_streams[0].format = StreamFormat::float32(2, true)),
        ),
        (
            "bytes per frame",
            sa,
            Box::new(|i| i.output_streams[0].format.bytes_per_frame = 4),
        ),
        (
            "bytes per packet",
            sa,
            Box::new(|i| i.output_streams[0].format.bytes_per_packet = 4),
        ),
        (
            "bits per channel",
            sa,
            Box::new(|i| i.output_streams[0].format.bits_per_channel = 24),
        ),
        (
            "frames per packet",
            sa,
            Box::new(|i| i.output_streams[0].format.frames_per_packet = 2),
        ),
        (
            "reserved",
            sa,
            Box::new(|i| i.output_streams[0].format.reserved = 1),
        ),
        (
            "format id",
            sa,
            Box::new(|i| i.output_streams[0].format.format_id = 0x61632d33),
        ),
        ("nominal rate", sl, Box::new(|i| i.nominal_rate = 44_100.0)),
    ];
    for (name, device, edit) in cases {
        let mut rig = Rig::new();
        rig.hal.edit(device, |info| edit(info));
        let result = rig.host.add_peer(peer(1), "mac");
        assert!(is_unsupported(result), "{name}: must be Unsupported");
        assert_eq!(rig.hal.listener_count(), 0, "{name}");
        assert_eq!(rig.hal.started_total(), 0, "{name}");
        rig.sync();
        assert!(rig.events.take().is_empty(), "{name}");
    }
}

#[test]
fn a_missing_plugin_is_unsupported_and_a_partial_one_is_a_mismatch() {
    let mut rig = Rig::new();
    rig.hal.remove_plugin();
    let error = rig.host.add_peer(peer(1), "mac").unwrap_err();
    assert!(
        matches!(&error, PlatformError::Unsupported(m) if m.contains("not installed")),
        "{error:?}"
    );
    assert_eq!(rig.hal.listener_count(), 0);

    let mut rig = Rig::new();
    rig.hal.set_translate(MIC_LOOPBACK_UID, None);
    let error = rig.host.add_peer(peer(1), "mac").unwrap_err();
    assert!(
        matches!(&error, PlatformError::Unsupported(m) if m.contains("do not match")),
        "{error:?}"
    );

    // Two UIDs resolving to one device is a mismatch too.
    let mut rig = Rig::new();
    rig.hal.set_translate(MIC_LOOPBACK_UID, Some(SPEAKERS_LOOP));
    assert!(is_unsupported(rig.host.add_peer(peer(1), "mac")));
}

#[test]
fn hal_failures_are_backend_errors_and_never_unsupported() {
    let mut rig = Rig::new();
    rig.hal.fail_translate(Some(HalError::Status(-1)));
    assert!(matches!(
        rig.host.add_peer(peer(1), "mac"),
        Err(PlatformError::Backend(_))
    ));
    rig.hal.fail_translate(None);
    rig.hal.fail_info(SPEAKERS_APP, Some(HalError::Status(-2)));
    assert!(matches!(
        rig.host.add_peer(peer(1), "mac"),
        Err(PlatformError::Backend(_))
    ));
}

#[test]
fn devices_are_found_by_exact_uid_only_never_by_order_or_default() {
    let mut rig = Rig::new();
    rig.hal.clear_calls();
    rig.host.add_peer(peer(1), "mac").unwrap();
    let calls = rig.hal.calls();
    let mut translated: Vec<String> = calls
        .iter()
        .filter_map(|c| match c {
            Call::Translate(uid) => Some(uid.clone()),
            _ => None,
        })
        .collect();
    translated.sort();
    translated.dedup();
    let mut expected = vec![
        SPEAKERS_APP_UID.to_string(),
        SPEAKERS_LOOPBACK_UID.to_string(),
        MIC_APP_UID.to_string(),
        MIC_LOOPBACK_UID.to_string(),
    ];
    expected.sort();
    assert_eq!(translated, expected);
    // Only the four Crosspane ids are ever read; the default output is never consulted, and no
    // IO exists without demand.
    for call in &calls {
        match call {
            Call::Info(d) | Call::Alive(d) | Call::Running(d) => assert!(
                [SPEAKERS_APP, SPEAKERS_LOOP, MIC_APP, MIC_LOOP].contains(d),
                "{call:?}"
            ),
            Call::DefaultOutput | Call::StartIo(_) => panic!("unexpected {call:?}"),
            _ => {}
        }
    }
}

// -- one peer, rebind ---------------------------------------------------------------------------

#[test]
fn a_second_peer_is_busy_and_the_same_peer_is_idempotent() {
    let (mut rig, first) = Rig::bound();
    let listeners = rig.hal.listener_count();
    assert!(listeners > 0);
    let error = rig.host.add_peer(peer(2), "other").unwrap_err();
    assert!(matches!(&error, PlatformError::Backend(m) if m == PEER_BUSY));
    // The same binding again: no duplicate listeners, no events, fresh ports.
    let second = rig.host.add_peer(peer(1), "mac").unwrap();
    assert_eq!(rig.hal.listener_count(), listeners);
    rig.sync();
    assert!(rig.events.take().is_empty());
    assert!(first.speaker_out.is_abandoned());
    assert!(!second.speaker_out.is_abandoned());
    // Removing a peer that is not bound is NotFound and changes nothing.
    assert!(matches!(
        rig.host.remove_peer(peer(2)),
        Err(PlatformError::NotFound)
    ));
    assert_eq!(rig.hal.listener_count(), listeners);
}

#[test]
fn rearming_while_forwarding_restarts_on_the_new_ring_without_events() {
    let (mut rig, mut old, _callback) = Rig::forwarding();
    let mut fresh = rig.host.add_peer(peer(1), "mac").unwrap();
    rig.sync();
    assert!(rig.events.take().is_empty());
    assert_eq!(rig.hal.active_on(SPEAKERS_LOOP), 1);
    let callback = rig.hal.callback(SPEAKERS_LOOP).unwrap();
    forward(&*callback, &stereo_pattern(0, 4));
    assert_eq!(drain(&mut fresh.speaker_out), stereo_pattern(0, 4));
    assert!(drain(&mut old.speaker_out).is_empty());
}

#[test]
fn every_rebind_after_a_detach_needs_both_visible_devices_inactive() {
    for same_peer in [false, true] {
        for busy in [SPEAKERS_APP, MIC_APP] {
            let (mut rig, _ports) = Rig::bound();
            rig.host.remove_peer(peer(1)).unwrap();
            let next = if same_peer { peer(1) } else { peer(2) };
            // An application is using a visible device while nothing is bound.
            rig.hal.app_start(busy);
            rig.hal.clear_calls();
            let error = rig.host.add_peer(next, "x").unwrap_err();
            let label = format!("same_peer={same_peer} busy={busy:?}");
            assert!(
                matches!(&error, PlatformError::Backend(m) if m == PEER_BUSY),
                "{label}: {error:?}"
            );
            // Nothing was published and the provisional listeners were retired again.
            assert_eq!(rig.hal.listener_count(), 0, "{label}");
            assert_eq!(
                rig.hal.count(|c| matches!(c, Call::AddListener(_))),
                rig.hal.count(|c| matches!(c, Call::RemoveListener(_))),
                "{label}"
            );
            assert_eq!(rig.hal.started_total(), 0, "{label}");
            rig.sync();
            assert!(rig.events.take().is_empty(), "{label}");
            // Once both are inactive a deliberate add_peer rebinds, with no event.
            rig.hal.app_stop(busy);
            assert!(rig.host.add_peer(next, "x").is_ok(), "{label}");
            rig.sync();
            assert!(rig.events.take().is_empty(), "{label}");
            assert_eq!(rig.hal.started_total(), 0, "{label}");
        }
    }
}

#[test]
fn a_visible_device_that_starts_during_the_rebind_is_caught_by_the_listener_backed_snapshot() {
    for same_peer in [false, true] {
        let (mut rig, _ports) = Rig::bound();
        rig.host.remove_peer(peer(1)).unwrap();
        let next = if same_peer { peer(1) } else { peer(2) };
        // The application starts after the devices were located (and found inactive) but while
        // the listeners are being installed, i.e. between any preliminary check and the snapshot.
        let hal = rig.hal.clone();
        let fired = Arc::new(AtomicBool::new(false));
        rig.hal.on_add_listener(move |_| {
            if !fired.swap(true, Ordering::SeqCst) {
                hal.app_start(SPEAKERS_APP);
            }
        });
        let error = rig.host.add_peer(next, "x").unwrap_err();
        assert!(
            matches!(&error, PlatformError::Backend(m) if m == PEER_BUSY),
            "same_peer={same_peer}: {error:?}"
        );
        assert_eq!(rig.hal.listener_count(), 0);
        assert_eq!(rig.hal.started_total(), 0);
        rig.sync();
        assert!(rig.events.take().is_empty());
    }
}

#[test]
fn the_first_bind_accepts_a_running_application_but_a_same_peer_rebind_does_not() {
    let mut rig = Rig::subscribed();
    rig.hal.app_start(SPEAKERS_APP);
    // A first bind (nothing was ever detached) with the speakers already running reports it and
    // forwards: an agent restart mid-playback still binds.
    rig.host.add_peer(peer(1), "mac").unwrap();
    assert_eq!(
        rig.events.take(),
        vec![active(peer(1), AudioKind::Speaker, true)]
    );
    // The loopback IOProc starts right after the command, outside the caller's bounded wait.
    rig.sync();
    assert_eq!(rig.hal.active_on(SPEAKERS_LOOP), 1);
    rig.host.remove_peer(peer(1)).unwrap();
    assert_eq!(
        rig.events.take(),
        vec![active(peer(1), AudioKind::Speaker, false)]
    );
    // The same peer comes back while the application still plays: refused.
    assert!(matches!(
        rig.host.add_peer(peer(1), "mac"),
        Err(PlatformError::Backend(m)) if m == PEER_BUSY
    ));
    rig.sync();
    assert!(rig.events.take().is_empty());
    assert_eq!(rig.hal.active_sessions(), 0);
    rig.hal.app_stop(SPEAKERS_APP);
    assert!(rig.host.add_peer(peer(1), "mac").is_ok());
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![active(peer(1), AudioKind::Speaker, true)]
    );
}

// -- subscribe / snapshot -------------------------------------------------------------------------

#[test]
fn listeners_are_registered_before_the_initial_running_snapshot() {
    let mut rig = Rig::subscribed();
    rig.hal.clear_calls();
    rig.host.add_peer(peer(1), "mac").unwrap();
    let calls = rig.hal.calls();
    let first_read = calls
        .iter()
        .position(|c| matches!(c, Call::Running(_)))
        .expect("snapshot reads");
    for visible in [SPEAKERS_APP, MIC_APP] {
        let listener = calls
            .iter()
            .position(|c| *c == Call::AddListener(ListenTarget::DeviceRunning(visible)))
            .expect("running listener");
        assert!(listener < first_read, "{visible:?} listener after snapshot");
    }
    // Hidden devices are never observed for running state.
    for hidden in [SPEAKERS_LOOP, MIC_LOOP] {
        assert!(!calls.contains(&Call::AddListener(ListenTarget::DeviceRunning(hidden))));
        assert!(!calls.contains(&Call::Running(hidden)));
    }
}

#[test]
fn a_change_between_listener_registration_and_snapshot_is_not_lost() {
    let mut rig = Rig::subscribed();
    let hal = rig.hal.clone();
    let fired = Arc::new(AtomicBool::new(false));
    rig.hal.on_running_read(move |device| {
        // The snapshot read has already returned "inactive"; an application starts right after.
        if device == SPEAKERS_APP && !fired.swap(true, Ordering::SeqCst) {
            hal.app_start(SPEAKERS_APP);
        }
    });
    rig.host.add_peer(peer(1), "mac").unwrap();
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![active(peer(1), AudioKind::Speaker, true)]
    );
    assert_eq!(rig.hal.active_on(SPEAKERS_LOOP), 1);
}

#[test]
fn subscribe_delivers_the_current_state_once_and_refuses_a_second_sink() {
    let mut rig = Rig::new();
    rig.host.add_peer(peer(1), "mac").unwrap();
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    rig.host.subscribe(rig.events.clone()).unwrap();
    assert_eq!(
        rig.events.take(),
        vec![
            active(peer(1), AudioKind::Speaker, true),
            active(peer(1), AudioKind::Microphone, false),
        ]
    );
    assert!(is_unsupported(
        rig.host.subscribe(Arc::new(Events::default()))
    ));
}

// -- demand ----------------------------------------------------------------------------------------

#[test]
fn visible_speaker_demand_starts_and_stops_exactly_the_hidden_speaker_ioproc() {
    let (rig, _ports) = Rig::bound();
    assert_eq!(rig.hal.started_total(), 0, "no app, no loopback IO");
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![active(peer(1), AudioKind::Speaker, true)]
    );
    assert_eq!(rig.hal.started_total(), 1);
    assert_eq!(rig.hal.active_on(SPEAKERS_LOOP), 1);
    rig.hal.app_stop(SPEAKERS_APP);
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![active(peer(1), AudioKind::Speaker, false)]
    );
    assert_eq!(rig.hal.active_sessions(), 0);
    assert_eq!(
        rig.hal.dropped_total(),
        1,
        "retired cleanly, state released"
    );
    // Again: a new session each time.
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    assert_eq!(rig.hal.started_total(), 2);
}

#[test]
fn hidden_device_activity_is_never_application_demand() {
    let (rig, _ports) = Rig::bound();
    // The hidden devices start and stop (as our own loopback IO would), and every listener fires.
    rig.hal.app_start(SPEAKERS_LOOP);
    rig.hal.app_start(MIC_LOOP);
    rig.hal.notify_all();
    rig.sync();
    assert!(rig.events.take().is_empty());
    assert_eq!(rig.hal.started_total(), 0);
    // Real demand starts our hidden IOProc; its own running state still adds nothing.
    rig.hal.app_stop(SPEAKERS_LOOP);
    rig.hal.app_stop(MIC_LOOP);
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    rig.hal.notify_all();
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![active(peer(1), AudioKind::Speaker, true)]
    );
}

#[test]
fn several_visible_starts_report_only_the_first_and_the_last_edge() {
    let (rig, _ports) = Rig::bound();
    rig.hal.app_start(SPEAKERS_APP);
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![active(peer(1), AudioKind::Speaker, true)]
    );
    rig.hal.app_stop(SPEAKERS_APP);
    rig.sync();
    assert!(rig.events.take().is_empty(), "one app is still running");
    assert_eq!(rig.hal.active_on(SPEAKERS_LOOP), 1);
    rig.hal.app_stop(SPEAKERS_APP);
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![active(peer(1), AudioKind::Speaker, false)]
    );
    assert_eq!(rig.hal.active_sessions(), 0);
}

#[test]
fn microphone_demand_is_a_level_and_never_starts_any_io() {
    let (rig, _ports) = Rig::bound();
    rig.hal.app_start(MIC_APP);
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![active(peer(1), AudioKind::Microphone, true)]
    );
    rig.hal.app_stop(MIC_APP);
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![active(peer(1), AudioKind::Microphone, false)]
    );
    assert_eq!(rig.hal.started_total(), 0);
    assert_eq!(rig.hal.started_on(MIC_LOOP), 0);
    assert_eq!(rig.hal.started_on(MIC_APP), 0);
}

// -- removal, barrier, rebind inactivity -------------------------------------------------------------

#[test]
fn remove_detaches_everything_and_nothing_from_the_old_binding_escapes() {
    let (mut rig, ports, callback) = Rig::forwarding();
    let late = rig.hal.notifiers();
    assert!(!late.is_empty());
    rig.host.remove_peer(peer(1)).unwrap();
    // The test's own handle on the retired callback is the last reference to its ring.
    drop(callback);
    // The inactive edge is the last event of the old binding.
    assert_eq!(
        rig.events.take(),
        vec![active(peer(1), AudioKind::Speaker, false)]
    );
    assert_eq!(rig.hal.listener_count(), 0);
    assert_eq!(rig.hal.active_sessions(), 0);
    assert!(ports.speaker_out.is_abandoned(), "the host dropped its end");
    // Late notifications on the retired binding do nothing at all.
    for notifier in &late {
        notifier.notify();
    }
    rig.hal.app_stop(SPEAKERS_APP);
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    assert!(rig.events.take().is_empty());
    assert_eq!(rig.hal.active_sessions(), 0);
    assert_eq!(
        rig.hal.started_total(),
        1,
        "no automatic rebinding or restart"
    );
}

#[test]
fn pending_pre_remove_events_are_drained_before_a_fresh_add_completes() {
    let (mut rig, _ports) = Rig::bound();
    // A notification is pending when the removal arrives.
    rig.hal.app_start(SPEAKERS_APP);
    rig.host.remove_peer(peer(1)).unwrap();
    assert_eq!(
        rig.events.take(),
        vec![
            active(peer(1), AudioKind::Speaker, true),
            active(peer(1), AudioKind::Speaker, false),
        ]
    );
    // A rebind needs the application to stop first; the fresh binding then starts clean.
    assert!(rig.host.add_peer(peer(1), "mac").is_err());
    rig.hal.app_stop(SPEAKERS_APP);
    rig.host.add_peer(peer(1), "mac").unwrap();
    rig.sync();
    assert!(rig.events.take().is_empty());
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![active(peer(1), AudioKind::Speaker, true)]
    );
}

// -- forwarding callback ----------------------------------------------------------------------------

#[test]
fn stereo_frames_keep_their_order_and_alignment_at_every_callback_size() {
    let (rig, mut ports, callback) = Rig::forwarding();
    let mut base = 0;
    for frames in [0, 1, 2, 3, 480, 511, 1024, 2400] {
        forward(&*callback, &stereo_pattern(base, frames));
        assert_eq!(
            drain(&mut ports.speaker_out),
            stereo_pattern(base, frames),
            "{frames} frames"
        );
        base += frames;
    }
    // The 4096-frame ceiling is accepted: the ring keeps the first 2400 whole frames.
    forward(&*callback, &stereo_pattern(0, 4096));
    assert_eq!(drain(&mut ports.speaker_out), stereo_pattern(0, 2400));
    let stats = callback.control().stats();
    assert_eq!(stats.frames_dropped, 4096 - 2400);
    rig.sync();
    assert!(rig.events.take().is_empty(), "no bad cycle was seen");
}

#[test]
fn a_cycle_above_the_ceiling_is_refused_whole_and_ends_the_session_with_failed() {
    let (rig, mut ports, callback) = Rig::forwarding();
    forward(&*callback, &stereo_pattern(0, 4097));
    assert!(
        drain(&mut ports.speaker_out).is_empty(),
        "no prefix is forwarded"
    );
    assert!(callback.control().bad_buffer_seen());
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![device_error(
            Some(peer(1)),
            AudioKind::Speaker,
            AudioDeviceError::Failed
        )]
    );
    assert_eq!(rig.hal.active_sessions(), 0);
    // Later cycles of the dead session do nothing; the next demand edge starts a fresh session.
    forward(&*callback, &stereo_pattern(0, 4));
    assert!(drain(&mut ports.speaker_out).is_empty());
    rig.hal.app_stop(SPEAKERS_APP);
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![active(peer(1), AudioKind::Speaker, false)]
    );
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![active(peer(1), AudioKind::Speaker, true)]
    );
    let fresh = rig.hal.callback(SPEAKERS_LOOP).unwrap();
    forward(&*fresh, &stereo_pattern(5, 3));
    assert_eq!(drain(&mut ports.speaker_out), stereo_pattern(5, 3));
}

#[test]
fn layouts_other_than_one_interleaved_stereo_buffer_are_refused() {
    type Make = Box<dyn Fn() -> Vec<Buf>>;
    let cases: Vec<(&str, Make)> = vec![
        ("mono", Box::new(|| vec![Buf::new(1, &[0.5; 8])])),
        ("three channels", Box::new(|| vec![Buf::new(3, &[0.5; 9])])),
        (
            "two planar buffers",
            Box::new(|| vec![Buf::new(1, &[0.5; 4]), Buf::new(1, &[0.5; 4])]),
        ),
        (
            "partial frame",
            Box::new(|| {
                let mut buf = Buf::new(2, &[0.5; 8]);
                buf.byte_size = 28;
                vec![buf]
            }),
        ),
        (
            "misaligned data",
            Box::new(|| {
                let mut buf = Buf::new(2, &[0.5; 8]);
                buf.skew = 1;
                vec![buf]
            }),
        ),
    ];
    for (name, make) in cases {
        let (rig, mut ports, callback) = Rig::forwarding();
        run_cycle(&*callback, &mut make(), &mut []);
        assert!(drain(&mut ports.speaker_out).is_empty(), "{name}");
        assert!(callback.control().bad_buffer_seen(), "{name}");
        rig.sync();
        assert_eq!(
            rig.events.take(),
            vec![device_error(
                Some(peer(1)),
                AudioKind::Speaker,
                AudioDeviceError::Failed
            )],
            "{name}"
        );
    }
}

#[test]
fn absent_data_is_not_an_error() {
    let (rig, mut ports, callback) = Rig::forwarding();
    run_cycle(&*callback, &mut [], &mut []);
    run_cycle(&*callback, &mut [Buf::null(2, 4096)], &mut []);
    run_cycle(&*callback, &mut [Buf::new(2, &[])], &mut []);
    assert!(drain(&mut ports.speaker_out).is_empty());
    assert!(!callback.control().bad_buffer_seen());
    forward(&*callback, &stereo_pattern(0, 2));
    assert_eq!(drain(&mut ports.speaker_out), stereo_pattern(0, 2));
    rig.sync();
    assert!(rig.events.take().is_empty());
}

#[test]
fn non_finite_samples_become_silence_without_rotating_channels() {
    let (_rig, mut ports, callback) = Rig::forwarding();
    forward(
        &*callback,
        &[f32::NAN, 1.0, 2.0, f32::INFINITY, f32::NEG_INFINITY, 3.0],
    );
    assert_eq!(
        drain(&mut ports.speaker_out),
        vec![0.0, 1.0, 2.0, 0.0, 0.0, 3.0]
    );
}

#[test]
fn overrun_drops_complete_frames_and_the_ring_never_misaligns() {
    let (_rig, mut ports, callback) = Rig::forwarding();
    // 2400 frames fit. Fill most of it, then overrun with a bigger cycle.
    forward(&*callback, &stereo_pattern(0, 1900));
    forward(&*callback, &stereo_pattern(1900, 1000));
    let stats = callback.control().stats();
    assert_eq!(stats.frames_moved, 2400);
    assert_eq!(stats.frames_dropped, 500);
    assert_eq!(drain(&mut ports.speaker_out), stereo_pattern(0, 2400));
    // A consumer that pops an odd number of samples leaves an odd number of free slots; the
    // producer still writes only whole frames.
    forward(&*callback, &stereo_pattern(0, 2400));
    for _ in 0..3 {
        ports.speaker_out.pop().unwrap();
    }
    forward(&*callback, &stereo_pattern(7000, 5));
    let after = callback.control().stats();
    assert_eq!(after.frames_moved - stats.frames_moved - 2400, 1);
    let rest = drain(&mut ports.speaker_out);
    assert_eq!(&rest[rest.len() - 2..], &[7000.0, -7000.0]);
}

#[test]
fn an_observed_gate_closure_latches_the_forwarder_and_reopening_alone_restarts_nothing() {
    let (rig, mut ports, callback) = Rig::forwarding();
    rig.gate.set_engine_permits(false);
    forward(&*callback, &stereo_pattern(0, 8));
    assert!(
        drain(&mut ports.speaker_out).is_empty(),
        "nothing retained while closed"
    );
    assert!(callback.control().gate_closed_seen());
    assert!(
        callback.control().is_disabled(),
        "latched at the observation"
    );
    // Reopen at once, before the owner has reconciled anything.
    rig.gate.set_engine_permits(true);
    forward(&*callback, &stereo_pattern(0, 8));
    assert!(
        drain(&mut ports.speaker_out).is_empty(),
        "the latched registration never forwards again"
    );
    wait_until("the latched forwarder to be retired", || {
        rig.hal.active_sessions() == 0
    });
    // However long the gate stays open, nothing starts.
    std::thread::sleep(Duration::from_millis(150));
    rig.sync();
    assert_eq!(rig.hal.started_total(), 1, "reopening restarts nothing");
    assert_eq!(rig.hal.active_sessions(), 0);
    forward(&*callback, &stereo_pattern(0, 8));
    assert!(drain(&mut ports.speaker_out).is_empty());
    assert!(
        rig.events.take().is_empty(),
        "the gate is the engine's business"
    );
    // A fresh demand cycle (inactive, reconciled, active again, gate open) starts a new one.
    rig.hal.app_stop(SPEAKERS_APP);
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![active(peer(1), AudioKind::Speaker, false)]
    );
    assert_eq!(rig.hal.started_total(), 1);
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![active(peer(1), AudioKind::Speaker, true)]
    );
    assert_eq!(rig.hal.started_total(), 2);
    let fresh = rig.hal.callback(SPEAKERS_LOOP).unwrap();
    forward(&*fresh, &stereo_pattern(3, 4));
    assert_eq!(drain(&mut ports.speaker_out), stereo_pattern(3, 4));
    // The old registration is still dead.
    forward(&*callback, &stereo_pattern(9, 4));
    assert!(drain(&mut ports.speaker_out).is_empty());
}

#[test]
fn the_latch_holds_while_the_owner_is_stuck_retiring_and_the_gate_reopens_meanwhile() {
    let (rig, mut ports, callback) = Rig::forwarding();
    // The owner's retirement of the forwarder is held inside the HAL's stop.
    rig.hal.slow_stop(Duration::from_millis(400));
    rig.gate.set_engine_permits(false);
    forward(&*callback, &stereo_pattern(0, 8));
    wait_until("the owner to be inside its retirement", || {
        rig.hal.stopping_now() == 1
    });
    // Close, call, reopen, call: all while the retirement is still held.
    forward(&*callback, &stereo_pattern(0, 8));
    rig.gate.set_engine_permits(true);
    forward(&*callback, &stereo_pattern(0, 8));
    assert!(drain(&mut ports.speaker_out).is_empty());
    assert!(callback.control().is_disabled());
    rig.sync();
    assert_eq!(rig.hal.stopped_total(), 1);
    assert_eq!(rig.hal.started_total(), 1);
    assert_eq!(rig.hal.active_sessions(), 0);
    // Retirement finished with the gate open: still nothing restarts, until a fresh cycle.
    std::thread::sleep(Duration::from_millis(80));
    rig.sync();
    assert_eq!(rig.hal.started_total(), 1);
    rig.hal.slow_stop(Duration::ZERO);
    rig.hal.app_stop(SPEAKERS_APP);
    rig.sync();
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    assert_eq!(rig.hal.started_total(), 2);
}

#[test]
fn a_closure_noticed_only_by_the_owner_latches_the_same_way() {
    let (rig, mut ports, callback) = Rig::forwarding();
    // No callback runs while the gate is closed: the owner's own gate poll sees it.
    rig.gate.set_session_permits(false);
    wait_until("the owner to retire the forwarder", || {
        rig.hal.active_sessions() == 0
    });
    rig.gate.set_session_permits(true);
    std::thread::sleep(Duration::from_millis(100));
    rig.sync();
    assert_eq!(rig.hal.started_total(), 1);
    assert!(callback.control().is_disabled());
    forward(&*callback, &stereo_pattern(0, 4));
    assert!(drain(&mut ports.speaker_out).is_empty());
}

#[test]
fn a_demand_edge_while_the_gate_is_closed_starts_nothing_until_a_fresh_cycle() {
    let (rig, _ports) = Rig::bound();
    rig.gate.set_session_permits(false);
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![active(peer(1), AudioKind::Speaker, true)]
    );
    assert_eq!(rig.hal.started_total(), 0);
    // The gate opens with the demand still active: that alone starts nothing.
    rig.gate.set_session_permits(true);
    std::thread::sleep(Duration::from_millis(100));
    rig.sync();
    assert_eq!(rig.hal.started_total(), 0);
    // Pause and play again with the gate open.
    rig.hal.app_stop(SPEAKERS_APP);
    rig.sync();
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![
            active(peer(1), AudioKind::Speaker, false),
            active(peer(1), AudioKind::Speaker, true),
        ]
    );
    assert_eq!(rig.hal.active_on(SPEAKERS_LOOP), 1);
}

/// Complete the cycle that proves nothing restarted by itself: the hidden loopback has been
/// started `started` times so far and stays there, then a fresh demand cycle starts exactly one
/// new forwarder, which forwards into `ports`.
fn assert_only_a_fresh_cycle_restarts(rig: &Rig, ports: &mut VirtualPorts, started: usize) {
    rig.sync();
    std::thread::sleep(Duration::from_millis(80));
    rig.sync();
    assert_eq!(
        rig.hal.started_on(SPEAKERS_LOOP),
        started,
        "nothing restarted by itself"
    );
    assert_eq!(rig.hal.active_sessions(), 0);
    rig.events.take();
    rig.hal.app_stop(SPEAKERS_APP);
    rig.sync();
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    assert_eq!(
        rig.hal.started_on(SPEAKERS_LOOP),
        started + 1,
        "the fresh cycle starts one"
    );
    let fresh = rig.hal.callback(SPEAKERS_LOOP).unwrap();
    forward(&*fresh, &stereo_pattern(5, 3));
    assert_eq!(drain(&mut ports.speaker_out), stereo_pattern(5, 3));
}

#[test]
fn a_same_peer_re_add_queued_before_supervision_cannot_bypass_an_observed_closure() {
    let (mut rig, _ports, callback) = Rig::forwarding();
    let playback = rig.host.open_playback(stereo()).unwrap();
    let probe = rig.host.queue_probe();
    // Stall the owner inside its service pass, in the HAL's stop of the playback it is retiring:
    // it has already supervised the forwarder in this pass and will not do so again before it
    // takes the next queued command.
    rig.hal.slow_stop(Duration::from_millis(700));
    drop(playback);
    wait_until("the owner to be stalled in the HAL", || {
        rig.hal.stopping_now() == 1
    });
    rig.hal.slow_stop(Duration::ZERO);
    // Meanwhile the running callback observes a closure and disables itself; the gate reopens.
    rig.gate.set_engine_permits(false);
    forward(&*callback, &stereo_pattern(0, 8));
    rig.gate.set_engine_permits(true);
    assert!(callback.control().gate_closed_seen());
    // A same-peer re-add is queued behind the stalled owner: it will run before any supervision.
    let mut fresh = std::thread::scope(|scope| {
        let re_add = scope.spawn(|| rig.host.add_peer(peer(1), "mac"));
        wait_until("the re-add to be queued behind the owner", || {
            probe.load(Ordering::SeqCst) == 1
        });
        re_add.join().unwrap().unwrap()
    });
    assert!(callback.control().is_disabled());
    assert_only_a_fresh_cycle_restarts(&rig, &mut fresh, 1);
}

#[test]
fn a_closure_observed_while_the_old_forwarder_is_being_retired_by_a_re_add_still_latches() {
    let (mut rig, _ports, callback) = Rig::forwarding();
    rig.hal.slow_stop(Duration::from_millis(400));
    let mut fresh = std::thread::scope(|scope| {
        let re_add = scope.spawn(|| rig.host.add_peer(peer(1), "mac"));
        wait_until("the owner to be inside the forwarder's retirement", || {
            rig.hal.stopping_now() == 1
        });
        // A callback that was still inside when retirement began sees the gate closed.
        callback.control().latch_gate_closed();
        re_add.join().unwrap().unwrap()
    });
    rig.hal.slow_stop(Duration::ZERO);
    assert_only_a_fresh_cycle_restarts(&rig, &mut fresh, 1);
}

#[test]
fn a_latch_survives_an_unclean_retirement_and_a_re_add() {
    let (mut rig, _ports, callback) = Rig::forwarding();
    callback.control().latch_gate_closed();
    // An outstanding callback makes the retirement unclean (the ring is poisoned).
    let held = callback.control().enter().expect("admission");
    let mut fresh = std::thread::scope(|scope| {
        let re_add = scope.spawn(|| rig.host.add_peer(peer(1), "mac"));
        re_add.join().unwrap().unwrap()
    });
    drop(held);
    assert_eq!(
        rig.events.take(),
        vec![device_error(
            Some(peer(1)),
            AudioKind::Speaker,
            AudioDeviceError::Failed
        )]
    );
    assert_only_a_fresh_cycle_restarts(&rig, &mut fresh, 1);
}

#[test]
fn an_unclean_retirement_latches_even_when_the_old_callbacks_latch_is_published_too_late() {
    let (mut rig, _ports, callback) = Rig::forwarding();
    // A callback is admitted and has seen the closed gate, but is paused before it publishes its
    // latch: the gate has reopened meanwhile, so nothing else shows the closure.
    let held = callback.control().enter().expect("admission");
    assert!(!callback.control().gate_closed_seen());
    // The re-add's retirement exceeds its drain bound (the admission is still held), so it is
    // unclean and must assume the unpublished closure.
    let mut fresh = std::thread::scope(|scope| {
        scope
            .spawn(|| rig.host.add_peer(peer(1), "mac"))
            .join()
            .unwrap()
            .unwrap()
    });
    assert!(
        !callback.control().gate_closed_seen(),
        "still unpublished at retirement"
    );
    // Only now does the old callback publish its latch, into state nobody reads again.
    callback.control().latch_gate_closed();
    drop(held);
    assert_eq!(
        rig.events.take(),
        vec![device_error(
            Some(peer(1)),
            AudioKind::Speaker,
            AudioDeviceError::Failed
        )]
    );
    // No replacement forwarder until a fresh inactive-to-active cycle.
    assert_only_a_fresh_cycle_restarts(&rig, &mut fresh, 1);
}

#[test]
fn a_bad_cycle_and_a_closure_seen_together_still_latch_across_a_re_add() {
    let (mut rig, _ports, callback) = Rig::forwarding();
    callback.control().mark_bad();
    callback.control().latch_gate_closed();
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![device_error(
            Some(peer(1)),
            AudioKind::Speaker,
            AudioDeviceError::Failed
        )]
    );
    let mut fresh = rig.host.add_peer(peer(1), "mac").unwrap();
    assert_only_a_fresh_cycle_restarts(&rig, &mut fresh, 1);
}

#[test]
fn a_deliberate_re_add_keeps_a_latched_forwarder_latched() {
    let (mut rig, _ports, callback) = Rig::forwarding();
    rig.gate.set_engine_permits(false);
    forward(&*callback, &stereo_pattern(0, 2));
    rig.gate.set_engine_permits(true);
    wait_until("the retirement", || rig.hal.active_sessions() == 0);
    let mut fresh = rig.host.add_peer(peer(1), "mac").unwrap();
    rig.sync();
    std::thread::sleep(Duration::from_millis(60));
    rig.sync();
    assert_eq!(rig.hal.started_total(), 1, "a re-add is not a demand cycle");
    // The new ring comes alive with the next fresh cycle.
    rig.hal.app_stop(SPEAKERS_APP);
    rig.sync();
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    let again = rig.hal.callback(SPEAKERS_LOOP).unwrap();
    forward(&*again, &stereo_pattern(4, 2));
    assert_eq!(drain(&mut fresh.speaker_out), stereo_pattern(4, 2));
}

#[test]
fn a_denied_loopback_start_reports_permission_denied_and_never_falls_back() {
    let (rig, _ports) = Rig::bound();
    rig.hal.fail_start(Some(HalError::Denied));
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![
            active(peer(1), AudioKind::Speaker, true),
            device_error(
                Some(peer(1)),
                AudioKind::Speaker,
                AudioDeviceError::PermissionDenied
            ),
        ]
    );
    rig.sync();
    // Not retried in a loop, and only the one device was ever attempted.
    assert_eq!(rig.hal.started_total(), 0);
    assert_eq!(rig.hal.count(|c| matches!(c, Call::StartIo(_))), 1);
    assert_eq!(rig.hal.started_on(SPEAKERS_LOOP), 1);
}

// -- loss ------------------------------------------------------------------------------------------

#[test]
fn losing_any_device_in_any_way_retires_everything_and_never_rebinds_by_itself() {
    #[derive(Clone, Copy, Debug)]
    enum Loss {
        /// `DeviceIsAlive` turns false.
        Dead,
        /// The object disappears: reads fail with `Gone`.
        Vanished,
        /// The UID no longer translates to any device.
        MissingUid,
        /// The UID translates to a different object id (the plug-in reloaded).
        ReplacedObject,
    }
    let devices = [
        (SPEAKERS_APP, SPEAKERS_APP_UID),
        (SPEAKERS_LOOP, SPEAKERS_LOOPBACK_UID),
        (MIC_APP, MIC_APP_UID),
        (MIC_LOOP, MIC_LOOPBACK_UID),
    ];
    for (device, uid) in devices {
        for loss in [
            Loss::Dead,
            Loss::Vanished,
            Loss::MissingUid,
            Loss::ReplacedObject,
        ] {
            let label = format!("{device:?} {loss:?}");
            let (mut rig, mut ports, callback) = Rig::forwarding();
            rig.hal.app_start(MIC_APP);
            rig.sync();
            assert_eq!(
                rig.events.take(),
                vec![active(peer(1), AudioKind::Microphone, true)]
            );
            match loss {
                Loss::Dead => rig.hal.kill(device),
                Loss::Vanished => rig.hal.vanish(device),
                Loss::MissingUid => {
                    rig.hal.set_translate(uid, None);
                    rig.hal.notify(ListenTarget::DeviceList);
                }
                Loss::ReplacedObject => rig.hal.replace_object(uid, DeviceId(900)),
            }
            rig.sync();
            // Demand ends, the error is reported once per kind, in a fixed order.
            assert_eq!(
                rig.events.take(),
                vec![
                    active(peer(1), AudioKind::Speaker, false),
                    active(peer(1), AudioKind::Microphone, false),
                    device_error(
                        Some(peer(1)),
                        AudioKind::Speaker,
                        AudioDeviceError::Unavailable
                    ),
                    device_error(
                        Some(peer(1)),
                        AudioKind::Microphone,
                        AudioDeviceError::Unavailable
                    ),
                ],
                "{label}"
            );
            // Retirement: the loopback IO is stopped and its callback is dead.
            assert_eq!(rig.hal.active_sessions(), 0, "{label}");
            assert!(callback.control().is_disabled(), "{label}");
            forward(&*callback, &stereo_pattern(0, 4));
            assert!(drain(&mut ports.speaker_out).is_empty(), "{label}");
            // No automatic rebind or restart, however much the HAL stirs.
            match loss {
                Loss::Dead => rig.hal.revive(device),
                Loss::MissingUid => rig.hal.set_translate(uid, Some(device)),
                Loss::Vanished | Loss::ReplacedObject => {}
            }
            rig.hal.notify_all();
            rig.sync();
            assert!(rig.events.take().is_empty(), "{label}");
            assert_eq!(rig.hal.started_total(), 1, "{label}");
            assert!(
                matches!(
                    rig.host.add_peer(peer(2), "other"),
                    Err(PlatformError::Backend(m)) if m == PEER_BUSY
                ),
                "{label}"
            );
            // Only a deliberate remove detaches the listeners.
            rig.host.remove_peer(peer(1)).unwrap();
            assert_eq!(rig.hal.listener_count(), 0, "{label}");
            assert!(rig.events.take().is_empty(), "{label}");
        }
    }
}

#[test]
fn a_lost_binding_re_added_by_the_same_peer_is_a_rebind_that_needs_inactivity() {
    let (mut rig, _ports, _callback) = Rig::forwarding();
    rig.hal.kill(SPEAKERS_APP);
    rig.sync();
    rig.events.take();
    rig.hal.revive(SPEAKERS_APP);
    // The speakers are still in use (the fake keeps its client): the deliberate re-add is refused.
    assert!(matches!(
        rig.host.add_peer(peer(1), "mac"),
        Err(PlatformError::Backend(m)) if m == PEER_BUSY
    ));
    assert_eq!(rig.hal.listener_count(), 0);
    rig.hal.app_stop(SPEAKERS_APP);
    let mut fresh = rig.host.add_peer(peer(1), "mac").unwrap();
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![active(peer(1), AudioKind::Speaker, true)]
    );
    let callback = rig.hal.callback(SPEAKERS_LOOP).unwrap();
    forward(&*callback, &stereo_pattern(1, 2));
    assert_eq!(drain(&mut fresh.speaker_out), stereo_pattern(1, 2));
}

#[test]
fn an_unrelated_notification_changes_nothing() {
    let (rig, _ports, _callback) = Rig::forwarding();
    rig.hal.notify(ListenTarget::DeviceList);
    rig.hal.notify(ListenTarget::ServiceRestarted);
    rig.sync();
    assert!(rig.events.take().is_empty());
    assert_eq!(rig.hal.active_on(SPEAKERS_LOOP), 1);
}

// -- retirement bounds ---------------------------------------------------------------------------------

#[test]
fn a_callback_still_inside_is_never_freed_and_its_ring_is_replaced_not_reused() {
    let (mut rig, mut ports, callback) = Rig::forwarding();
    // An IO thread is "inside" the callback when demand ends.
    let held = callback.control().enter().expect("admission");
    rig.hal.app_stop(SPEAKERS_APP);
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![
            device_error(Some(peer(1)), AudioKind::Speaker, AudioDeviceError::Failed),
            active(peer(1), AudioKind::Speaker, false),
        ]
    );
    // The IOProc was stopped, but the session state was kept alive (never dropped).
    assert_eq!(rig.hal.stopped_total(), 1);
    assert_eq!(rig.hal.dropped_total(), 0, "leaked on purpose");
    drop(held);
    forward(&*callback, &stereo_pattern(0, 4));
    assert!(
        drain(&mut ports.speaker_out).is_empty(),
        "disabled callbacks write nothing"
    );
    // The ring may still be referenced, so forwarding is not restarted on it...
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![active(peer(1), AudioKind::Speaker, true)]
    );
    assert_eq!(rig.hal.active_sessions(), 0);
    // ...until a deliberate re-add provides a fresh one.
    let mut fresh = rig.host.add_peer(peer(1), "mac").unwrap();
    rig.sync();
    assert_eq!(rig.hal.active_on(SPEAKERS_LOOP), 1);
    let again = rig.hal.callback(SPEAKERS_LOOP).unwrap();
    forward(&*again, &stereo_pattern(9, 2));
    assert_eq!(drain(&mut fresh.speaker_out), stereo_pattern(9, 2));
}

#[test]
fn a_notification_still_queued_when_the_binding_is_removed_is_flushed_boundedly_and_isolated() {
    let (mut rig, _ports) = Rig::bound();
    // A listener block is running (stuck) on the HAL's queue when the removal arrives.
    let latch = rig.hal.hold_next_notification();
    rig.hal.app_start(SPEAKERS_APP);
    assert_eq!(rig.hal.pending_notifications(), 1);
    let started = Instant::now();
    let result = rig.host.remove_peer(peer(1));
    let elapsed = started.elapsed();
    // The flush is bounded (about 100 ms), reports the stuck block, and the removal still happens.
    assert!(matches!(result, Err(PlatformError::Timeout)), "{result:?}");
    assert!(
        elapsed >= Duration::from_millis(90) && elapsed < Duration::from_secs(1),
        "{elapsed:?}"
    );
    assert!(rig.hal.count(|c| matches!(c, Call::Flush)) >= 1);
    assert_eq!(rig.hal.listener_count(), 0);
    assert_eq!(rig.hal.pending_notifications(), 1, "still queued");
    assert!(rig.events.take().is_empty());
    // A fresh binding is possible (the application stopped meanwhile).
    rig.hal.app_stop(SPEAKERS_APP);
    rig.host.add_peer(peer(1), "mac").unwrap();
    // The old notification now completes, late: it reaches only the retired notifier.
    latch.release();
    wait_until("the late notification to finish", || {
        rig.hal.pending_notifications() == 0
    });
    rig.sync();
    assert!(
        rig.events.take().is_empty(),
        "no event from the old binding"
    );
    assert_eq!(rig.hal.active_sessions(), 0);
    // The fresh binding is unaffected and works on its own.
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![active(peer(1), AudioKind::Speaker, true)]
    );
    assert_eq!(rig.hal.active_on(SPEAKERS_LOOP), 1);
}

#[test]
fn retirement_waits_for_an_outstanding_callback_while_forwarding_continues_correctly() {
    let (rig, mut ports, callback) = Rig::forwarding();
    let stop = Arc::new(AtomicBool::new(false));
    let flowed = Arc::new(AtomicUsize::new(0));
    let hammer = {
        let callback = callback.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                forward(&*callback, &stereo_pattern(0, 64));
                std::thread::yield_now();
            }
        })
    };
    let drainer = {
        let stop = stop.clone();
        let flowed = flowed.clone();
        std::thread::spawn(move || {
            let mut samples = Vec::new();
            while !stop.load(Ordering::SeqCst) {
                samples.extend(drain(&mut ports.speaker_out));
                flowed.store(samples.len(), Ordering::SeqCst);
                std::thread::sleep(Duration::from_micros(200));
            }
            samples.extend(drain(&mut ports.speaker_out));
            samples
        })
    };
    // Handshake: frames really are flowing before retirement starts.
    wait_until("forwarded frames", || flowed.load(Ordering::SeqCst) > 0);
    // An outstanding callback: this thread is "inside" between the hammer's cycles.
    let held = loop {
        if let Some(admission) = callback.control().enter() {
            break admission;
        }
        std::thread::yield_now();
    };
    rig.hal.app_stop(SPEAKERS_APP);
    wait_until("the owner to stop the IOProc", || {
        rig.hal.stopped_total() == 1
    });
    std::thread::sleep(Duration::from_millis(10));
    assert_eq!(
        rig.hal.dropped_total(),
        0,
        "the state is kept while a callback is outstanding"
    );
    drop(held);
    rig.sync();
    assert_eq!(
        rig.hal.dropped_total(),
        1,
        "released once the callback left"
    );
    assert_eq!(rig.hal.active_sessions(), 0);
    stop.store(true, Ordering::SeqCst);
    hammer.join().unwrap();
    let samples = drainer.join().unwrap();
    assert!(
        !samples.is_empty(),
        "frames were forwarded before retirement"
    );
    assert!(samples.len().is_multiple_of(2), "whole callbacks only");
    for frame in samples.as_chunks::<2>().0 {
        assert_eq!(frame[0], -frame[1], "channels stayed paired");
    }
    assert_eq!(
        rig.events.take(),
        vec![active(peer(1), AudioKind::Speaker, false)],
        "a clean retirement, no failure"
    );
}

// -- physical playback --------------------------------------------------------------------------------

fn stereo() -> AudioFormat {
    AudioKind::Speaker.format()
}

fn out(frames: usize) -> Vec<Buf> {
    vec![Buf::marked(2, frames * 2)]
}

fn planar(frames: usize) -> Vec<Buf> {
    vec![Buf::marked(1, frames), Buf::marked(1, frames)]
}

#[test]
fn playback_renders_interleaved_frames_with_silence_on_underrun() {
    let mut rig = Rig::subscribed();
    let mut handle = rig.host.open_playback(stereo()).unwrap();
    assert_eq!(rig.hal.started_on(BUILTIN), 1);
    let callback = rig.hal.callback(BUILTIN).unwrap();
    for sample in stereo_pattern(0, 3) {
        handle.pcm.push(sample).unwrap();
    }
    let mut output = out(5);
    run_cycle(&*callback, &mut [], &mut output);
    // Three buffered frames play in order, the rest is silence (whole frames).
    let mut expected = stereo_pattern(0, 3);
    expected.extend([0.0; 4]);
    assert_eq!(output[0].samples(), expected);
    // Nothing buffered: a full cycle of silence, never stale memory.
    let mut output = out(480);
    run_cycle(&*callback, &mut [], &mut output);
    assert!(output[0].samples().iter().all(|s| *s == 0.0));
    // A zero-frame cycle is a no-op.
    run_cycle(&*callback, &mut [], &mut [Buf::new(2, &[])]);
    assert!(!callback.control().bad_buffer_seen());
}

#[test]
fn playback_renders_planar_frames_and_keeps_stereo_alignment() {
    let hal_rig = Rig::new();
    hal_rig.hal.edit(BUILTIN, |info| {
        info.output_streams[0].format = StreamFormat::float32(2, false)
    });
    let mut host = hal_rig.host;
    let mut handle = host.open_playback(stereo()).unwrap();
    let callback = hal_rig.hal.callback(BUILTIN).unwrap();
    for sample in stereo_pattern(10, 4) {
        handle.pcm.push(sample).unwrap();
    }
    let mut output = planar(6);
    run_cycle(&*callback, &mut [], &mut output);
    assert_eq!(output[0].samples(), vec![10.0, 11.0, 12.0, 13.0, 0.0, 0.0]);
    assert_eq!(
        output[1].samples(),
        vec![-10.0, -11.0, -12.0, -13.0, 0.0, 0.0]
    );
}

#[test]
fn playback_keeps_order_across_many_cycles_and_ring_wraparound() {
    let mut rig = Rig::new();
    let mut handle = rig.host.open_playback(stereo()).unwrap();
    let callback = rig.hal.callback(BUILTIN).unwrap();
    let mut next_in = 0;
    let mut next_out = 0;
    for round in 0..40 {
        let frames = 300 + (round * 37) % 500;
        let queued = frames.min(handle.pcm.slots() / 2);
        for sample in stereo_pattern(next_in, queued) {
            handle.pcm.push(sample).unwrap();
        }
        next_in += queued;
        let mut output = out(frames);
        run_cycle(&*callback, &mut [], &mut output);
        let samples = output[0].samples();
        let played = queued.min(frames);
        let expected_from = next_out;
        assert_eq!(
            &samples[..played * 2],
            &stereo_pattern(expected_from, played)[..],
            "round {round}"
        );
        next_out += played;
        assert!(samples[played * 2..].iter().all(|s| *s == 0.0));
    }
}

#[test]
fn playback_sanitises_non_finite_samples_and_rejects_foreign_layouts() {
    let mut rig = Rig::subscribed();
    let mut handle = rig.host.open_playback(stereo()).unwrap();
    let callback = rig.hal.callback(BUILTIN).unwrap();
    for sample in [f32::NAN, 1.0, f32::INFINITY, 2.0] {
        handle.pcm.push(sample).unwrap();
    }
    let mut output = out(2);
    run_cycle(&*callback, &mut [], &mut output);
    assert_eq!(output[0].samples(), vec![0.0, 1.0, 0.0, 2.0]);
    // A planar list on an interleaved handle: silence, bad, Failed, handle closed.
    let mut wrong = planar(4);
    run_cycle(&*callback, &mut [], &mut wrong);
    assert!(wrong.iter().all(|b| b.samples().iter().all(|s| *s == 0.0)));
    assert!(callback.control().bad_buffer_seen());
    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![device_error(
            None,
            AudioKind::Speaker,
            AudioDeviceError::Failed
        )]
    );
    assert_eq!(rig.hal.active_sessions(), 0);
}

#[test]
fn playback_buffers_that_differ_from_the_validated_layout_are_silenced_and_fail_the_handle() {
    type Make = Box<dyn Fn() -> Vec<Buf>>;
    for planar_handle in [false, true] {
        let cases: Vec<(&str, bool, Make)> = if planar_handle {
            vec![
                ("interleaved pair on planar", true, Box::new(|| out(4))),
                (
                    "single mono buffer",
                    true,
                    Box::new(|| vec![Buf::marked(1, 4)]),
                ),
                (
                    "three planar buffers",
                    true,
                    Box::new(|| vec![Buf::marked(1, 4), Buf::marked(1, 4), Buf::marked(1, 4)]),
                ),
                (
                    "unequal planar buffers",
                    true,
                    Box::new(|| vec![Buf::marked(1, 4), Buf::marked(1, 3)]),
                ),
                (
                    "two stereo buffers",
                    true,
                    Box::new(|| vec![Buf::marked(2, 8), Buf::marked(2, 8)]),
                ),
            ]
        } else {
            vec![
                ("mono", true, Box::new(|| vec![Buf::marked(1, 8)])),
                (
                    "three channels",
                    true,
                    Box::new(|| vec![Buf::marked(3, 12)]),
                ),
                ("planar pair on interleaved", true, Box::new(|| planar(4))),
                (
                    "two stereo buffers",
                    true,
                    Box::new(|| vec![Buf::marked(2, 8), Buf::marked(2, 8)]),
                ),
                (
                    "partial frame",
                    true,
                    Box::new(|| {
                        let mut buf = Buf::marked(2, 8);
                        buf.byte_size = 28;
                        vec![buf]
                    }),
                ),
                (
                    "misaligned data",
                    false,
                    Box::new(|| {
                        let mut buf = Buf::marked(2, 8);
                        buf.skew = 1;
                        vec![buf]
                    }),
                ),
            ]
        };
        for (name, check_silence, make) in cases {
            let mut rig = Rig::subscribed();
            if planar_handle {
                rig.hal.edit(BUILTIN, |info| {
                    info.output_streams[0].format = StreamFormat::float32(2, false)
                });
            }
            let _handle = rig.host.open_playback(stereo()).unwrap();
            let callback = rig.hal.callback(BUILTIN).unwrap();
            let mut buffers = make();
            run_cycle(&*callback, &mut [], &mut buffers);
            assert!(callback.control().bad_buffer_seen(), "{name}");
            if check_silence {
                for buffer in &buffers {
                    assert!(buffer.samples().iter().all(|s| *s == 0.0), "{name}");
                }
            }
            rig.sync();
            assert_eq!(
                rig.events.take(),
                vec![device_error(
                    None,
                    AudioKind::Speaker,
                    AudioDeviceError::Failed
                )],
                "{name}"
            );
            assert_eq!(rig.hal.active_sessions(), 0, "{name}");
        }
    }
}

#[test]
fn a_reentrant_playback_invocation_writes_silence_instead_of_stale_memory() {
    let mut rig = Rig::new();
    let mut handle = rig.host.open_playback(stereo()).unwrap();
    let callback = rig.hal.callback(BUILTIN).unwrap();
    for sample in stereo_pattern(0, 4) {
        handle.pcm.push(sample).unwrap();
    }
    // Another invocation is already inside (which a well-behaved HAL never does).
    let inside = callback.control().enter().expect("admission");
    let mut output = out(4);
    run_cycle(&*callback, &mut [], &mut output);
    assert!(output[0].samples().iter().all(|s| *s == 0.0));
    drop(inside);
    // Nothing was consumed by the refused invocation.
    let mut output = out(4);
    run_cycle(&*callback, &mut [], &mut output);
    assert_eq!(output[0].samples(), stereo_pattern(0, 4));
}

#[test]
fn planar_output_buffers_must_be_disjoint_address_ranges() {
    // (name, left offset, right offset, both 32 bytes long, accepted)
    let cases: [(&str, usize, usize, bool); 7] = [
        ("identical pointers", 0, 0, false),
        ("partial overlap by one sample", 0, 4, false),
        ("partial overlap by half", 0, 16, false),
        ("one starts inside the other, reversed", 16, 0, false),
        ("adjacent, disjoint", 0, 32, true),
        ("adjacent, reversed", 32, 0, true),
        ("a gap between them", 0, 64, true),
    ];
    for (name, left_at, right_at, accepted) in cases {
        let mut rig = Rig::subscribed();
        rig.hal.edit(BUILTIN, |info| {
            info.output_streams[0].format = StreamFormat::float32(2, false)
        });
        let mut handle = rig.host.open_playback(stereo()).unwrap();
        let callback = rig.hal.callback(BUILTIN).unwrap();
        for sample in stereo_pattern(1, 8) {
            handle.pcm.push(sample).unwrap();
        }
        let mut storage = vec![7.0f32; 32];
        let base = storage.as_mut_ptr().cast::<u8>();
        let buffer = |offset: usize| IoBuffer {
            channels: 1,
            byte_size: 32,
            // SAFETY: offsets are at most 64 bytes into a 128-byte allocation.
            data: unsafe { base.add(offset) }.cast(),
        };
        let (left, right) = (buffer(left_at), buffer(right_at));
        // SAFETY: both ranges lie inside `storage`, which outlives the call.
        unsafe { run_cycle_raw(&*callback, &[], &[left, right]) };
        if accepted {
            assert!(!callback.control().bad_buffer_seen(), "{name}");
            let at = |offset: usize| -> Vec<f32> { storage[offset / 4..offset / 4 + 8].to_vec() };
            assert_eq!(
                at(left_at),
                (1..=8).map(|v| v as f32).collect::<Vec<_>>(),
                "{name}"
            );
            assert_eq!(
                at(right_at),
                (1..=8).map(|v| -(v as f32)).collect::<Vec<_>>(),
                "{name}"
            );
        } else {
            assert!(callback.control().bad_buffer_seen(), "{name}");
            assert_eq!(handle.pcm.slots(), 4800 - 16, "{name}: nothing consumed");
            rig.sync();
            assert_eq!(
                rig.events.take(),
                vec![device_error(
                    None,
                    AudioKind::Speaker,
                    AudioDeviceError::Failed
                )],
                "{name}"
            );
        }
    }
}

#[test]
fn buffers_whose_address_range_would_wrap_are_rejected_without_being_touched() {
    let wild = (usize::MAX & !3) as *mut std::ffi::c_void;
    // Forwarding input.
    let (rig, mut ports, callback) = Rig::forwarding();
    let input = IoBuffer {
        channels: 2,
        byte_size: 64,
        data: wild,
    };
    // SAFETY: the callback rejects the wrapping range before any access.
    unsafe { run_cycle_raw(&*callback, &[input], &[]) };
    assert!(callback.control().bad_buffer_seen());
    assert!(drain(&mut ports.speaker_out).is_empty());
    rig.sync();
    // Playback output, interleaved and planar.
    for planar_handle in [false, true] {
        let mut rig = Rig::new();
        if planar_handle {
            rig.hal.edit(BUILTIN, |info| {
                info.output_streams[0].format = StreamFormat::float32(2, false)
            });
        }
        let _handle = rig.host.open_playback(stereo()).unwrap();
        let callback = rig.hal.callback(BUILTIN).unwrap();
        let buffers = if planar_handle {
            vec![
                IoBuffer {
                    channels: 1,
                    byte_size: 32,
                    data: wild,
                },
                IoBuffer {
                    channels: 1,
                    byte_size: 32,
                    data: wild,
                },
            ]
        } else {
            vec![IoBuffer {
                channels: 2,
                byte_size: 64,
                data: wild,
            }]
        };
        // SAFETY: the callback rejects the wrapping range before any access (and never writes).
        unsafe { run_cycle_raw(&*callback, &[], &buffers) };
        assert!(
            callback.control().bad_buffer_seen(),
            "planar={planar_handle}"
        );
    }
}

#[test]
fn a_disabled_output_stream_is_not_an_error() {
    let mut rig = Rig::subscribed();
    let _handle = rig.host.open_playback(stereo()).unwrap();
    let callback = rig.hal.callback(BUILTIN).unwrap();
    // A disabled stream reports its size with a null pointer; nothing to write, nothing wrong.
    run_cycle(&*callback, &mut [], &mut [Buf::null(2, 4096)]);
    run_cycle(&*callback, &mut [], &mut []);
    assert!(!callback.control().bad_buffer_seen());
    rig.sync();
    assert!(rig.events.take().is_empty());
    assert_eq!(rig.hal.active_on(BUILTIN), 1);
}

#[test]
fn playback_cycles_above_the_ceiling_or_with_partial_frames_are_refused() {
    for frames_bytes in [4097usize * 8, 12] {
        let mut rig = Rig::subscribed();
        let _handle = rig.host.open_playback(stereo()).unwrap();
        let callback = rig.hal.callback(BUILTIN).unwrap();
        let mut buf = Buf::marked(2, frames_bytes.div_ceil(4));
        buf.byte_size = frames_bytes as u32;
        run_cycle(&*callback, &mut [], std::slice::from_mut(&mut buf));
        assert!(callback.control().bad_buffer_seen(), "{frames_bytes}");
        assert!(buf.samples().iter().all(|s| *s == 0.0), "{frames_bytes}");
        rig.sync();
        assert_eq!(
            rig.events.take(),
            vec![device_error(
                None,
                AudioKind::Speaker,
                AudioDeviceError::Failed
            )]
        );
    }
}

#[test]
fn a_virtual_default_output_is_rejected_without_any_io() {
    for crosspane in [SPEAKERS_APP, SPEAKERS_LOOP, MIC_APP, MIC_LOOP] {
        let mut rig = Rig::new();
        rig.hal.set_default_output(Some(crosspane));
        rig.hal.clear_calls();
        assert!(is_unsupported(rig.host.open_playback(stereo())));
        assert_eq!(rig.hal.started_total(), 0);
        assert_eq!(rig.hal.listener_count(), 0);
    }
    // Also by the UID read back from an otherwise unremarkable device id.
    let mut rig = Rig::new();
    rig.hal.edit(BUILTIN, |info| {
        info.uid = SPEAKERS_APP_UID.to_string();
        info.transport = TRANSPORT_BUILTIN;
    });
    assert!(is_unsupported(rig.host.open_playback(stereo())));
    assert_eq!(rig.hal.started_total(), 0);
}

#[test]
fn unsupported_hardware_formats_never_reach_an_ioproc() {
    type Edit = Box<dyn Fn(&mut DeviceInfo)>;
    let cases: Vec<(&str, Edit)> = vec![
        (
            "44.1 kHz stream",
            Box::new(|i| i.output_streams[0].format.sample_rate = 44_100.0),
        ),
        ("44.1 kHz device", Box::new(|i| i.nominal_rate = 44_100.0)),
        (
            "16-bit integer",
            Box::new(|i| {
                let f = &mut i.output_streams[0].format;
                f.format_flags = 12;
                f.bits_per_channel = 16;
                f.bytes_per_frame = 4;
                f.bytes_per_packet = 4;
            }),
        ),
        (
            "24-bit packed integer",
            Box::new(|i| {
                let f = &mut i.output_streams[0].format;
                f.format_flags = 12;
                f.bits_per_channel = 24;
                f.bytes_per_frame = 6;
                f.bytes_per_packet = 6;
            }),
        ),
        (
            "mono",
            Box::new(|i| i.output_streams[0].format = StreamFormat::float32(1, true)),
        ),
        (
            "six channels",
            Box::new(|i| i.output_streams[0].format = StreamFormat::float32(6, true)),
        ),
        (
            "two streams",
            Box::new(|i| {
                let extra = i.output_streams[0];
                i.output_streams.push(extra);
            }),
        ),
        ("no output stream", Box::new(|i| i.output_streams.clear())),
        ("not alive", Box::new(|i| i.alive = false)),
        (
            "bytes per frame",
            Box::new(|i| i.output_streams[0].format.bytes_per_frame = 16),
        ),
        (
            "frames per packet",
            Box::new(|i| i.output_streams[0].format.frames_per_packet = 4),
        ),
    ];
    for (name, edit) in cases {
        let mut rig = Rig::new();
        rig.hal.edit(BUILTIN, |info| edit(info));
        assert!(is_unsupported(rig.host.open_playback(stereo())), "{name}");
        assert_eq!(rig.hal.started_total(), 0, "{name}");
        assert_eq!(
            rig.hal.count(|c| matches!(c, Call::StartIo(_))),
            0,
            "{name}"
        );
        assert_eq!(rig.hal.listener_count(), 0, "{name}");
    }
}

#[test]
fn playback_refuses_foreign_requests_and_a_closed_gate_before_touching_the_hal() {
    let mut rig = Rig::new();
    rig.hal.clear_calls();
    for format in [
        AudioFormat {
            rate: 44_100,
            channels: 2,
        },
        AudioFormat {
            rate: 48_000,
            channels: 1,
        },
        AudioFormat {
            rate: 48_000,
            channels: 6,
        },
    ] {
        assert!(is_unsupported(rig.host.open_playback(format)));
    }
    rig.gate.set_engine_permits(false);
    assert!(matches!(
        rig.host.open_playback(stereo()),
        Err(PlatformError::Locked)
    ));
    assert!(rig.hal.calls().is_empty());
    // No default output at all.
    rig.gate.set_engine_permits(true);
    rig.hal.set_default_output(None);
    assert!(matches!(
        rig.host.open_playback(stereo()),
        Err(PlatformError::NotFound)
    ));
    assert_eq!(rig.hal.started_total(), 0);
}

#[test]
fn an_observed_gate_closure_latches_silence_and_closes_the_handle() {
    let mut rig = Rig::subscribed();
    let mut handle = rig.host.open_playback(stereo()).unwrap();
    let callback = rig.hal.callback(BUILTIN).unwrap();
    for sample in stereo_pattern(0, 8) {
        handle.pcm.push(sample).unwrap();
    }
    let mut output = out(2);
    run_cycle(&*callback, &mut [], &mut output);
    assert_eq!(output[0].samples(), stereo_pattern(0, 2));
    let free_before = handle.pcm.slots();

    rig.gate.set_engine_permits(false);
    let mut output = out(2);
    run_cycle(&*callback, &mut [], &mut output);
    assert!(output[0].samples().iter().all(|s| *s == 0.0));
    assert!(callback.control().gate_closed_seen());
    // Reopening does not un-latch: silence, and no sample is consumed.
    rig.gate.set_engine_permits(true);
    let mut output = out(2);
    run_cycle(&*callback, &mut [], &mut output);
    assert!(output[0].samples().iter().all(|s| *s == 0.0));
    assert_eq!(handle.pcm.slots(), free_before);

    rig.sync();
    assert_eq!(
        rig.events.take(),
        vec![device_error(
            None,
            AudioKind::Speaker,
            AudioDeviceError::Locked
        )]
    );
    assert_eq!(rig.hal.active_sessions(), 0);
    // Nothing restarts it; a deliberate reopen works with the gate open again.
    rig.sync();
    assert_eq!(rig.hal.started_total(), 1);
    let _second = rig.host.open_playback(stereo()).unwrap();
    assert_eq!(rig.hal.started_total(), 2);
}

#[test]
fn a_gate_closure_between_callbacks_is_still_noticed_by_the_owner() {
    let mut rig = Rig::subscribed();
    let _handle = rig.host.open_playback(stereo()).unwrap();
    rig.gate.set_session_permits(false);
    let events = rig.events.wait_for(1);
    assert_eq!(
        events,
        vec![device_error(
            None,
            AudioKind::Speaker,
            AudioDeviceError::Locked
        )]
    );
    assert_eq!(rig.hal.active_sessions(), 0);
}

#[test]
fn stop_and_drop_silence_at_once_and_finish_within_fifty_milliseconds() {
    let mut rig = Rig::new();
    let mut handle = rig.host.open_playback(stereo()).unwrap();
    let callback = rig.hal.callback(BUILTIN).unwrap();
    for sample in stereo_pattern(0, 4) {
        handle.pcm.push(sample).unwrap();
    }
    let started = Instant::now();
    drop(handle);
    assert!(
        started.elapsed() < Duration::from_millis(50),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(rig.hal.stopped_total(), 1);
    assert_eq!(rig.hal.active_sessions(), 0);
    assert_eq!(rig.hal.listener_count(), 0);
    let mut output = out(4);
    run_cycle(&*callback, &mut [], &mut output);
    assert!(output[0].samples().iter().all(|s| *s == 0.0));
}

#[test]
fn a_slow_hal_cannot_stretch_stop_past_the_bound_and_samples_stop_immediately() {
    let mut rig = Rig::new();
    let mut handle = rig.host.open_playback(stereo()).unwrap();
    let callback = rig.hal.callback(BUILTIN).unwrap();
    for sample in stereo_pattern(0, 4) {
        handle.pcm.push(sample).unwrap();
    }
    rig.hal.slow_stop(Duration::from_millis(400));
    let started = Instant::now();
    drop(handle);
    assert!(
        started.elapsed() < Duration::from_millis(50),
        "{:?}",
        started.elapsed()
    );
    // The callback is already silent and consumes nothing, though the HAL stop is still running.
    let mut output = out(4);
    run_cycle(&*callback, &mut [], &mut output);
    assert!(output[0].samples().iter().all(|s| *s == 0.0));
    wait_until("the owner to finish retirement", || {
        rig.hal.stopped_total() == 1 && rig.hal.dropped_total() == 1
    });
}

#[test]
fn losing_or_changing_the_default_output_closes_the_handle_without_reopening() {
    type Change = Box<dyn Fn(&FakeHal)>;
    let cases: Vec<(&str, Change)> = vec![
        (
            "default changed",
            Box::new(|hal| hal.set_default_output(Some(OTHER_OUTPUT))),
        ),
        (
            "default removed",
            Box::new(|hal| hal.set_default_output(None)),
        ),
        ("device lost", Box::new(|hal| hal.kill(BUILTIN))),
        (
            "sample rate changed",
            Box::new(|hal| {
                hal.edit(BUILTIN, |i| i.nominal_rate = 44_100.0);
                hal.notify(ListenTarget::DeviceNominalRate(BUILTIN));
            }),
        ),
        (
            "stream format changed",
            Box::new(|hal| {
                hal.edit(BUILTIN, |i| {
                    i.output_streams[0].format = StreamFormat::float32(2, false)
                });
                hal.notify(ListenTarget::StreamFormat(StreamId(2001)));
            }),
        ),
    ];
    for (name, change) in cases {
        let mut rig = Rig::subscribed();
        let mut handle = rig.host.open_playback(stereo()).unwrap();
        let callback = rig.hal.callback(BUILTIN).unwrap();
        change(&rig.hal);
        let events = rig.events.wait_for(1);
        assert_eq!(
            events,
            vec![device_error(
                None,
                AudioKind::Speaker,
                AudioDeviceError::Unavailable
            )],
            "{name}"
        );
        assert_eq!(rig.hal.active_sessions(), 0, "{name}");
        assert_eq!(rig.hal.listener_count(), 0, "{name}");
        rig.sync();
        assert_eq!(
            rig.hal.started_total(),
            1,
            "{name}: never reopened by itself"
        );
        // The dead handle is still safe to feed and drop.
        let _ = handle.pcm.push(0.0);
        let mut output = out(1);
        run_cycle(&*callback, &mut [], &mut output);
        assert!(output[0].samples().iter().all(|s| *s == 0.0), "{name}");
        drop(handle);
    }
}

#[test]
fn unrelated_notifications_do_not_close_a_healthy_handle() {
    let mut rig = Rig::subscribed();
    let _handle = rig.host.open_playback(stereo()).unwrap();
    rig.hal.notify_all();
    rig.sync();
    assert!(rig.events.take().is_empty());
    assert_eq!(rig.hal.active_on(BUILTIN), 1);
}

#[test]
fn the_playback_handle_count_is_bounded() {
    let mut rig = Rig::new();
    let mut handles = Vec::new();
    while let Ok(handle) = rig.host.open_playback(stereo()) {
        handles.push(handle);
        assert!(handles.len() <= 16, "unbounded");
    }
    assert!(!handles.is_empty());
    assert!(is_unsupported(rig.host.open_playback(stereo())));
}

// -- bounded operations, late success, host drop --------------------------------------------------------

#[test]
fn a_late_playback_open_is_stopped_after_the_caller_timed_out() {
    let mut rig = Rig::new();
    let latch = rig.hal.hold_start();
    let started = Instant::now();
    let result = rig.host.open_playback(stereo());
    assert!(matches!(result, Err(PlatformError::Timeout)));
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1900) && elapsed < Duration::from_millis(2600),
        "{elapsed:?}"
    );
    latch.release();
    wait_until("the late success to be retired", || {
        rig.hal.started_total() == 1 && rig.hal.stopped_total() == 1
    });
    // Retirement continues past the IOProc stop (listeners are removed after it): wait for the
    // owner to finish the whole command before looking at them.
    rig.sync();
    assert_eq!(rig.hal.listener_count(), 0);
    // The host is healthy afterwards.
    assert!(rig.host.open_playback(stereo()).is_ok());
}

#[test]
fn an_add_peer_the_caller_gave_up_on_is_cancelled_before_it_binds() {
    let mut rig = Rig::subscribed();
    let latch = rig.hal.hold_translate();
    let result = rig.host.add_peer(peer(1), "mac");
    assert!(matches!(result, Err(PlatformError::Timeout)));
    latch.release();
    rig.sync();
    assert_eq!(rig.hal.listener_count(), 0);
    assert!(rig.host.add_peer(peer(1), "mac").is_ok());
}

#[test]
fn a_late_add_peer_success_is_undone_and_a_retry_succeeds() {
    let mut rig = Rig::subscribed();
    // Stall the initial snapshot, which runs after the cancellation check, so the bind completes
    // after the caller timed out.
    let snapshot = fake_hal::Latch::new_closed();
    let snapshot_hold = snapshot.clone();
    let blocked = Arc::new(AtomicBool::new(false));
    rig.hal.on_running_read(move |_| {
        if !blocked.swap(true, Ordering::SeqCst) {
            snapshot_hold.wait();
        }
    });
    let result = rig.host.add_peer(peer(1), "mac");
    assert!(matches!(result, Err(PlatformError::Timeout)));
    snapshot.release();
    wait_until("the late bind to be undone", || {
        rig.sync();
        rig.hal.listener_count() == 0
    });
    assert!(rig.events.take().is_empty());
    assert!(rig.host.add_peer(peer(1), "mac").is_ok());
}

#[test]
fn dropping_the_host_silences_every_callback_even_when_the_first_retirement_is_stuck() {
    let (mut rig, mut ports, forwarder) = Rig::forwarding();
    let mut first = rig.host.open_playback(stereo()).unwrap();
    let mut second = rig.host.open_playback(stereo()).unwrap();
    let playbacks = rig.hal.callbacks(BUILTIN);
    assert_eq!(playbacks.len(), 2);
    for handle in [&mut first, &mut second] {
        for sample in stereo_pattern(0, 8) {
            handle.pcm.push(sample).unwrap();
        }
    }
    let free = (first.pcm.slots(), second.pcm.slots());
    // Every IOProc stop takes far longer than a host drop may wait.
    rig.hal.slow_stop(Duration::from_millis(2600));
    let Rig { hal, host, .. } = rig;
    let started = Instant::now();
    drop(host);
    assert!(
        started.elapsed() < Duration::from_millis(2500),
        "{:?}",
        started.elapsed()
    );
    // Drop returned with the first retirement still stuck inside the HAL...
    assert!(hal.stopping_now() >= 1);
    // ...yet no surviving callback moves a sample.
    forward(&*forwarder, &stereo_pattern(0, 8));
    assert!(drain(&mut ports.speaker_out).is_empty());
    for playback in &playbacks {
        let mut output = out(4);
        run_cycle(&**playback, &mut [], &mut output);
        assert!(output[0].samples().iter().all(|s| *s == 0.0));
    }
    assert_eq!(
        (first.pcm.slots(), second.pcm.slots()),
        free,
        "nothing consumed"
    );
}

#[test]
fn dropping_the_host_silences_callbacks_even_when_the_owner_is_stuck_elsewhere() {
    let (mut rig, mut ports, forwarder) = Rig::forwarding();
    let mut handle = rig.host.open_playback(stereo()).unwrap();
    let playback = rig.hal.callback(BUILTIN).unwrap();
    for sample in stereo_pattern(0, 8) {
        handle.pcm.push(sample).unwrap();
    }
    let free = handle.pcm.slots();
    // The owner is stuck inside an unrelated retirement (the forwarder's) when the drop arrives.
    rig.hal.slow_stop(Duration::from_millis(2600));
    rig.hal.app_stop(SPEAKERS_APP);
    wait_until("the owner to be stuck in the HAL", || {
        rig.hal.stopping_now() == 1
    });
    let Rig { hal, host, .. } = rig;
    drop(host);
    assert!(
        hal.stopping_now() >= 1,
        "the owner never reached its teardown"
    );
    // The playback was never disabled by the owner; the host's shutdown flag silences it.
    let mut output = out(4);
    run_cycle(&*playback, &mut [], &mut output);
    assert!(output[0].samples().iter().all(|s| *s == 0.0));
    assert_eq!(handle.pcm.slots(), free, "nothing consumed");
    forward(&*forwarder, &stereo_pattern(0, 8));
    assert!(drain(&mut ports.speaker_out).is_empty());
}

#[test]
fn dropping_the_host_retires_everything_and_late_events_are_ignored() {
    let mut rig = Rig::subscribed();
    let _ports = rig.host.add_peer(peer(1), "mac").unwrap();
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    let handle = rig.host.open_playback(stereo()).unwrap();
    rig.events.take();
    let late = rig.hal.notifiers();
    let Rig {
        hal, host, events, ..
    } = rig;
    let started = Instant::now();
    drop(host);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(hal.listener_count(), 0);
    assert_eq!(hal.active_sessions(), 0);
    assert_eq!(hal.dropped_total(), 2);
    for notifier in late {
        notifier.notify();
    }
    drop(handle);
    std::thread::sleep(Duration::from_millis(50));
    assert!(events.take().is_empty());
}

// -- microphone: nothing, ever --------------------------------------------------------------------------

#[test]
fn capture_is_always_unsupported_and_never_reaches_the_hal_or_the_gate() {
    let mut rig = Rig::new();
    rig.hal.clear_calls();
    for open in [true, false] {
        rig.gate.set_engine_permits(open);
        for format in [
            AudioKind::Microphone.format(),
            AudioKind::Speaker.format(),
            AudioFormat {
                rate: 44_100,
                channels: 9,
            },
        ] {
            assert!(
                is_unsupported(rig.host.open_capture(format)),
                "open={open} {format:?}"
            );
        }
    }
    assert!(rig.hal.calls().is_empty());
}

#[test]
fn the_microphone_port_is_never_written_or_read_through_demand_callbacks_gate_and_rearming() {
    let (mut rig, mut ports) = Rig::bound();
    let capacity = ports.mic_in.buffer().capacity();
    assert_eq!(ports.mic_in.slots(), capacity, "returned empty");
    // The agent's own samples: a host that read the ring would free this space again.
    for _ in 0..100 {
        ports.mic_in.push(0.25).unwrap();
    }
    let untouched = |ports: &VirtualPorts, written: usize| {
        assert_eq!(ports.mic_in.slots(), capacity - written, "mic ring changed");
        assert!(!ports.mic_in.is_abandoned());
    };
    untouched(&ports, 100);
    // Demand changes on both devices, forwarding callbacks, gate changes.
    rig.hal.app_start(MIC_APP);
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    let callback = rig.hal.callback(SPEAKERS_LOOP).unwrap();
    forward(&*callback, &stereo_pattern(0, 16));
    run_cycle(&*callback, &mut [Buf::new(1, &[0.5; 8])], &mut []);
    rig.gate.set_engine_permits(false);
    forward(&*callback, &stereo_pattern(0, 16));
    rig.gate.set_engine_permits(true);
    rig.sync();
    untouched(&ports, 100);
    rig.hal.app_stop(MIC_APP);
    rig.hal.app_stop(SPEAKERS_APP);
    rig.sync();
    let _playback = rig.host.open_playback(stereo()).unwrap();
    assert!(is_unsupported(
        rig.host.open_capture(AudioKind::Microphone.format())
    ));
    untouched(&ports, 100);
    // Rearming hands out a fresh, empty microphone port and abandons the old one.
    let mut again = rig.host.add_peer(peer(1), "mac").unwrap();
    assert!(ports.mic_in.is_abandoned());
    assert_eq!(again.mic_in.slots(), capacity);
    for _ in 0..7 {
        again.mic_in.push(0.5).unwrap();
    }
    rig.hal.app_start(MIC_APP);
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    let callback = rig.hal.callback(SPEAKERS_LOOP).unwrap();
    forward(&*callback, &stereo_pattern(0, 16));
    rig.hal.app_stop(MIC_APP);
    rig.sync();
    untouched(&again, 7);
    // Removal drops the host's end only: the agent's producer sees it abandoned, never filled.
    rig.host.remove_peer(peer(1)).unwrap();
    assert_eq!(again.mic_in.slots(), capacity - 7);
}

#[test]
fn no_scenario_ever_touches_a_microphone_device_or_port() {
    let (mut rig, _ports) = Rig::bound();
    rig.hal.app_start(MIC_APP);
    rig.hal.app_start(SPEAKERS_APP);
    rig.sync();
    let handle = rig.host.open_playback(stereo()).unwrap();
    rig.hal.app_stop(MIC_APP);
    rig.hal.app_stop(SPEAKERS_APP);
    rig.sync();
    assert!(is_unsupported(
        rig.host.open_capture(AudioKind::Microphone.format())
    ));
    drop(handle);
    rig.host.remove_peer(peer(1)).unwrap();
    // IO was only ever created on the hidden speakers loopback and the default output.
    let started: Vec<_> = rig
        .hal
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::StartIo(d) => Some(d),
            _ => None,
        })
        .collect();
    assert!(!started.is_empty());
    for device in started {
        assert!([SPEAKERS_LOOP, BUILTIN].contains(&device), "{device:?}");
    }
    assert_eq!(rig.hal.started_on(MIC_APP), 0);
    assert_eq!(rig.hal.started_on(MIC_LOOP), 0);
}

// -- the real host constructs without touching the HAL --------------------------------------------------

#[test]
fn the_real_host_builds_refuses_capture_and_makes_no_hal_call_for_refused_opens() {
    let gate = IoGate::new();
    let mut host = CoreAudioHost::new(gate.clone()).unwrap();
    // Closed gate, then wrong format, then capture: all answered before any HAL call.
    assert!(matches!(
        host.open_playback(AudioKind::Speaker.format()),
        Err(PlatformError::Locked)
    ));
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    assert!(is_unsupported(host.open_playback(AudioFormat {
        rate: 48_000,
        channels: 1
    })));
    assert!(is_unsupported(
        host.open_capture(AudioKind::Microphone.format())
    ));
    drop(host);
}

// -- public SDK ABI ------------------------------------------------------------------------------------

/// Compiles a C program with the SDK's own headers that prints every layout fact and constant the
/// FFI module declares, and compares them with the Rust values. Skipped (with a reason) when no C
/// toolchain is installed.
#[test]
fn ffi_layouts_and_constants_match_the_public_sdk() {
    use std::process::Command;
    let report = crosspane_platform_macos::audio::sdk_abi_report();
    assert!(report.len() > 40);
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("wp35-abi");
    std::fs::create_dir_all(&dir).unwrap();
    let mut source = String::from(
        "#include <CoreAudio/CoreAudio.h>\n#include <CoreFoundation/CoreFoundation.h>\n#include <stddef.h>\n#include <stdio.h>\n#define V(x) printf(\"%s=%llu\\n\", #x, (unsigned long long)(x))\nint main(void) {\n",
    );
    for (name, _) in &report {
        source.push_str(&format!("    V({name});\n"));
    }
    source.push_str("    return 0;\n}\n");
    let c_file = dir.join("abi.c");
    let binary = dir.join("abi");
    std::fs::write(&c_file, source).unwrap();
    let compile = Command::new("xcrun")
        .args(["--sdk", "macosx", "clang", "-std=gnu17", "-Wall", "-Werror"])
        .arg(&c_file)
        .args([
            "-framework",
            "CoreAudio",
            "-framework",
            "CoreFoundation",
            "-o",
        ])
        .arg(&binary)
        .output();
    let compile = match compile {
        Ok(output) => output,
        Err(error) => {
            eprintln!("SKIPPED: no C toolchain ({error})");
            return;
        }
    };
    assert!(
        compile.status.success(),
        "the SDK check program failed to compile:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let run = Command::new(&binary).output().unwrap();
    assert!(run.status.success());
    let printed = String::from_utf8(run.stdout).unwrap();
    let sdk: std::collections::HashMap<&str, u64> = printed
        .lines()
        .map(|line| {
            let (name, value) = line.rsplit_once('=').unwrap();
            (name, value.parse().unwrap())
        })
        .collect();
    assert_eq!(sdk.len(), report.len());
    for (name, ours) in report {
        assert_eq!(sdk.get(name), Some(&ours), "{name}");
    }
}

// -- owner-attended probes (never run by the acceptance commands) ---------------------------------------
//
// These two tests are the prepared owner-run probe. They are `#[ignore]`d and additionally need
// `CROSSPANE_AUDIO_OWNER_ATTENDED=1`, so no automated run ever reaches the real HAL. They are for the
// owner, in a Mac login session, after installing the signed plug-in with WP-3.4's script:
//
//   CROSSPANE_AUDIO_OWNER_ATTENDED=1 cargo test -p crosspane-platform-macos --test audio \
//       -- --ignored --nocapture owner_attended
//
// The speaker probe binds the real devices and, for 20 s, reports demand events and the level of
// whatever is played to "Crosspane speakers" (pick it in System Settings > Sound > Output, or in an
// app's output menu, and play something). Expect `VirtualActive{Speaker, true}` and non-zero PCM;
// macOS may show a microphone indicator or prompt for the hidden loopback input, and a denial must
// appear as `DeviceError` (never as silent success). Starting a second app must not change the
// state; stopping the last one must produce `VirtualActive{Speaker, false}`. The tone probe plays a
// quiet 440 Hz tone for one second on the default output when it is 48 kHz float32 stereo, and
// prints why not otherwise. Neither changes any default device.

fn owner_attended() -> bool {
    if std::env::var("CROSSPANE_AUDIO_OWNER_ATTENDED").as_deref() != Ok("1") {
        eprintln!("SKIPPED: owner-attended probe; set CROSSPANE_AUDIO_OWNER_ATTENDED=1");
        return false;
    }
    true
}

#[test]
#[ignore = "owner-attended: needs the installed signed Crosspane audio plug-in"]
fn owner_attended_speaker_loopback_probe() {
    if !owner_attended() {
        return;
    }
    let mut host = CoreAudioHost::new(open_gate()).unwrap();
    let events = Arc::new(Events::default());
    host.subscribe(events.clone()).unwrap();
    let bound = peer(0xAA);
    let mut ports = match host.add_peer(bound, "owner probe") {
        Ok(ports) => ports,
        Err(error) => {
            eprintln!("add_peer refused: {error:?}");
            return;
        }
    };
    eprintln!("bound; play sound to 'Crosspane speakers' now (20 s)");
    let (mut samples, mut peak) = (0usize, 0f32);
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        while let Ok(sample) = ports.speaker_out.pop() {
            samples += 1;
            peak = peak.max(sample.abs());
        }
    }
    eprintln!("events: {:?}", events.take());
    eprintln!("forwarded {samples} samples, peak {peak}");
    host.remove_peer(bound).unwrap();
}

#[test]
#[ignore = "owner-attended: plays a quiet tone on the real default output"]
fn owner_attended_default_output_tone() {
    if !owner_attended() {
        return;
    }
    let mut host = CoreAudioHost::new(open_gate()).unwrap();
    let mut handle = match host.open_playback(stereo()) {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("open_playback refused: {error:?}");
            return;
        }
    };
    let mut frame = 0usize;
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        while handle.pcm.slots() >= 2 {
            let value = 0.05 * (frame as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin();
            handle.pcm.push(value).unwrap();
            handle.pcm.push(value).unwrap();
            frame += 1;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    eprintln!("played {frame} frames");
}
