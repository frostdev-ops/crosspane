#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::collections::{BTreeMap, BTreeSet};

use crosspane_engine::io::{AudioEndpoint, AudioKey};
use crosspane_engine::{Command, Engine, EngineConfig, Failure, Input, Notice, Output};
use crosspane_input::journal::MemoryJournal;
use crosspane_platform::{
    AudioDeviceError, AudioEvent, HotkeyEvent, LockState, SessionEvent, SessionState,
};
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::{Capability, ControlMessage, Refusal};
use crosspane_types::audio::{AudioKind, AudioStreamId};
use crosspane_types::id::NodeId;
use crosspane_types::time::MonoTime;

const LOCAL: NodeId = NodeId([1; 32]);
const PEER: NodeId = NodeId([2; 32]);
const OTHER: NodeId = NodeId([3; 32]);
const KINDS: [AudioKind; 2] = [AudioKind::Speaker, AudioKind::Microphone];
fn ms(n: u64) -> MonoTime {
    MonoTime::from_nanos(n * 1_000_000)
}
fn run(e: &mut Engine, i: Input) -> Vec<Output> {
    e.handle(i, ms(0))
}
/// The production configuration: speakers only, microphones unsupported (WP-3.0b).
fn engine(node: NodeId) -> Engine {
    let (mut e, _) = Engine::new(
        EngineConfig::new(node),
        Box::<MemoryJournal>::default(),
        Box::<MemoryJournal>::default(),
        ms(0),
    )
    .unwrap();
    run(&mut e, unlocked());
    e
}
/// Synthetic positive microphone tests enable support explicitly; production never does.
fn mic_engine(node: NodeId) -> Engine {
    let mut e = engine(node);
    assert!(only(run(&mut e, microphones(true))).is_empty());
    e
}
fn microphones(available: bool) -> Input {
    Input::AudioMicrophoneSupport { available }
}
fn unlocked() -> Input {
    Input::Session(SessionEvent::State(SessionState {
        lock: LockState::Unlocked,
        active: Some(true),
    }))
}
fn feature(peer: NodeId, available: bool) -> Input {
    Input::AudioPeer {
        peer,
        name: "display text".into(),
        available,
    }
}
fn connect(e: &mut Engine, peer: NodeId) {
    run(e, Input::PeerUp { peer });
    run(e, feature(peer, true));
}
fn grants(peers: &[NodeId]) -> Input {
    Input::Grants(
        peers
            .iter()
            .map(|p| {
                (
                    *p,
                    BTreeSet::from([Capability::AudioSpeaker, Capability::AudioMic]),
                )
            })
            .collect(),
    )
}
/// A connected, fully granted engine with the synthetic microphone support enabled.
fn prepared() -> Engine {
    let mut e = mic_engine(LOCAL);
    connect(&mut e, PEER);
    run(&mut e, grants(&[PEER]));
    e
}
/// The same, in the production configuration: microphones unsupported.
fn speaker_only() -> Engine {
    let mut e = engine(LOCAL);
    connect(&mut e, PEER);
    run(&mut e, grants(&[PEER]));
    e
}
fn control(peer: NodeId, msg: ControlMessage) -> Input {
    Input::Link(LinkEvent::Control { peer, msg })
}
fn open(peer: NodeId, id: u16, kind: AudioKind) -> Input {
    control(
        peer,
        ControlMessage::AudioOpen {
            stream: AudioStreamId(id),
            kind,
            channels: kind.format().channels as u8,
        },
    )
}
fn activity(peer: NodeId, kind: AudioKind, active: bool) -> Input {
    Input::Audio(AudioEvent::VirtualActive { peer, kind, active })
}
fn shown(key: AudioKey, visible: bool) -> Input {
    Input::AudioIndicatorShown { key, visible }
}
fn done(key: AudioKey, kind: AudioKind, result: Result<(), Failure>) -> Input {
    Input::AudioDeviceOpened { key, kind, result }
}
fn key(outputs: &[Output]) -> AudioKey {
    let mic_peer = outputs.iter().find_map(|o| match o {
        Output::Notice(Notice::MicInUseBy(peer)) => Some(*peer),
        _ => None,
    });
    outputs
        .iter()
        .find_map(|o| match o {
            Output::OpenAudioPlayback { key } => Some(*key),
            Output::AudioIndicators { microphones, .. } => microphones
                .iter()
                .find(|k| Some(k.peer) == mic_peer)
                .copied(),
            _ => None,
        })
        .unwrap()
}
fn outgoing(outputs: &[Output]) -> AudioStreamId {
    outputs
        .iter()
        .find_map(|o| match o {
            Output::SendControl {
                msg: ControlMessage::AudioOpen { stream, .. },
                ..
            } => Some(*stream),
            _ => None,
        })
        .unwrap()
}
fn audio_output(o: &Output) -> bool {
    matches!(
        o,
        Output::AddAudioPeer { .. }
            | Output::RemoveAudioPeer { .. }
            | Output::OpenAudioCapture { .. }
            | Output::CloseAudioCapture { .. }
            | Output::OpenAudioPlayback { .. }
            | Output::CloseAudioPlayback { .. }
            | Output::StartAudioStream { .. }
            | Output::StopAudioStream { .. }
            | Output::AudioIndicators { .. }
            | Output::Notice(
                Notice::AudioRefused { .. } | Notice::MicInUseBy(_) | Notice::SpeakerInUseBy(_)
            )
            | Output::SendControl {
                msg: ControlMessage::AudioOpen { .. }
                    | ControlMessage::AudioOpened { .. }
                    | ControlMessage::AudioClose { .. }
                    | ControlMessage::AudioRefused { .. },
                ..
            }
    )
}
fn only(out: Vec<Output>) -> Vec<Output> {
    out.into_iter().filter(audio_output).collect()
}
fn no_start_or_capture(out: &[Output]) {
    assert!(!out.iter().any(|o| matches!(
        o,
        Output::OpenAudioCapture { .. } | Output::StartAudioStream { .. }
    )));
}
fn refused(out: &[Output], reason: Refusal) {
    assert!(out.iter().any(
        |o| matches!(o, Output::Notice(Notice::AudioRefused { reason: r, .. }) if *r == reason)
    ));
    no_start_or_capture(out);
}
fn closed(out: &[Output], key: AudioKey, kind: AudioKind) {
    let stop = out
        .iter()
        .position(|o| matches!(o, Output::StopAudioStream { key: k } if *k == key))
        .unwrap();
    let close = out
        .iter()
        .position(|o| match kind {
            AudioKind::Microphone => matches!(o, Output::CloseAudioCapture { key: k } if *k == key),
            AudioKind::Speaker => matches!(o, Output::CloseAudioPlayback { key: k } if *k == key),
        })
        .unwrap();
    let cleared = out.iter().position(|o| matches!(o, Output::AudioIndicators { microphones, speakers } if !microphones.contains(&key) && !speakers.contains(&key))).unwrap();
    assert!(stop < close && close < cleared, "{out:?}");
}
fn incoming(e: &mut Engine, id: u16, kind: AudioKind, active: bool) -> AudioKey {
    let k = key(&run(e, open(PEER, id, kind)));
    if kind == AudioKind::Microphone {
        assert!(run(e, shown(k, true)).contains(&Output::OpenAudioCapture { key: k }));
    }
    if active {
        let out = run(e, done(k, kind, Ok(())));
        assert!(out.contains(&Output::StartAudioStream {
            key: k,
            kind,
            endpoint: if kind == AudioKind::Microphone {
                AudioEndpoint::LocalCapture
            } else {
                AudioEndpoint::LocalPlayback
            }
        }));
    }
    k
}

#[test]
fn devices_only_after_authenticated_feature_negotiation() {
    let mut e = engine(LOCAL);
    assert!(only(run(&mut e, feature(PEER, true))).is_empty());
    assert!(only(run(&mut e, Input::PeerUp { peer: PEER })).is_empty());
    assert_eq!(
        only(run(&mut e, feature(PEER, true))),
        vec![Output::AddAudioPeer {
            peer: PEER,
            name: "display text".into()
        }]
    );
    assert!(only(run(&mut e, feature(PEER, true))).is_empty());
    assert_eq!(
        only(run(&mut e, feature(PEER, false))),
        vec![Output::RemoveAudioPeer { peer: PEER }]
    );
    assert!(only(run(&mut e, feature(PEER, false))).is_empty());
    run(&mut e, feature(PEER, true));
    assert_eq!(
        only(run(
            &mut e,
            Input::Link(LinkEvent::Closed {
                peer: PEER,
                error: LinkError::Closed
            })
        )),
        vec![Output::RemoveAudioPeer { peer: PEER }]
    );
}

#[test]
fn both_kinds_both_directions_and_both_id_parities() {
    for local in [LOCAL, OTHER] {
        for kind in KINDS {
            let mut e = mic_engine(local);
            connect(&mut e, PEER);
            run(&mut e, grants(&[PEER]));
            let out = run(&mut e, activity(PEER, kind, true));
            let stream = outgoing(&out);
            assert_eq!(stream.0, if local < PEER { 1 } else { 2 });
            assert!(!out.iter().any(|o| matches!(
                o,
                Output::OpenAudioCapture { .. } | Output::OpenAudioPlayback { .. }
            )));
            let out = run(
                &mut e,
                control(PEER, ControlMessage::AudioOpened { stream }),
            );
            assert!(out.iter().any(|o| matches!(o, Output::StartAudioStream { kind: k, endpoint, .. } if *k == kind && *endpoint == if kind == AudioKind::Speaker { AudioEndpoint::VirtualSpeaker } else { AudioEndpoint::VirtualMicrophone })));
            let k = key(&run(
                &mut e,
                open(PEER, if local < PEER { 2 } else { 1 }, kind),
            ));
            if kind == AudioKind::Microphone {
                run(&mut e, shown(k, true));
            }
            let out = run(&mut e, done(k, kind, Ok(())));
            let start = out
                .iter()
                .position(|o| matches!(o, Output::StartAudioStream { key, .. } if *key == k))
                .unwrap();
            let ack = out.iter().position(|o| matches!(o, Output::SendControl { msg: ControlMessage::AudioOpened { stream }, .. } if *stream == k.stream)).unwrap();
            assert!(start < ack);
        }
    }
}

#[test]
fn default_off_unnegotiated_invalid_and_replayed_requests_cannot_capture() {
    for kind in KINDS {
        let mut e = mic_engine(LOCAL);
        refused(&run(&mut e, open(PEER, 2, kind)), Refusal::Permission);
        run(&mut e, Input::PeerUp { peer: PEER });
        run(&mut e, grants(&[PEER]));
        refused(&run(&mut e, open(PEER, 4, kind)), Refusal::Permission);
        run(&mut e, feature(PEER, true));
        run(&mut e, grants(&[]));
        refused(&run(&mut e, open(PEER, 6, kind)), Refusal::Permission);
        run(&mut e, grants(&[PEER]));
        for id in [0, 1, 6] {
            refused(&run(&mut e, open(PEER, id, kind)), Refusal::Permission);
        }
        refused(
            &run(
                &mut e,
                control(
                    PEER,
                    ControlMessage::AudioOpen {
                        stream: AudioStreamId(8),
                        kind,
                        channels: 3,
                    },
                ),
            ),
            Refusal::Permission,
        );
        refused(&run(&mut e, open(PEER, 8, kind)), Refusal::Permission);
        let k = key(&run(&mut e, open(PEER, 10, kind)));
        refused(&run(&mut e, open(PEER, 12, kind)), Refusal::Busy);
        refused(&run(&mut e, open(PEER, 10, kind)), Refusal::Permission);
        run(
            &mut e,
            control(PEER, ControlMessage::AudioClose { stream: k.stream }),
        );
        refused(&run(&mut e, open(PEER, 12, kind)), Refusal::Permission);
        assert!(key(&run(&mut e, open(PEER, 14, kind))).generation > k.generation);
    }
}

#[test]
fn microphone_waits_for_visible_indicator_and_visibility_loss_stops() {
    for active in [false, true] {
        let mut e = prepared();
        let out = run(&mut e, open(PEER, 2, AudioKind::Microphone));
        let k = key(&out);
        let notice = out
            .iter()
            .position(|o| *o == Output::Notice(Notice::MicInUseBy(PEER)))
            .unwrap();
        let indicator = out
            .iter()
            .position(
                |o| matches!(o, Output::AudioIndicators { microphones, .. } if microphones == &[k]),
            )
            .unwrap();
        assert!(notice < indicator);
        no_start_or_capture(&out);
        assert_eq!(
            only(run(&mut e, shown(k, true))),
            vec![Output::OpenAudioCapture { key: k }]
        );
        assert!(only(run(&mut e, shown(k, true))).is_empty());
        if active {
            run(&mut e, done(k, AudioKind::Microphone, Ok(())));
        }
        let out = run(&mut e, shown(k, false));
        closed(&out, k, AudioKind::Microphone);
        no_start_or_capture(&run(&mut e, shown(k, true)));
        assert_eq!(
            only(run(&mut e, done(k, AudioKind::Microphone, Ok(())))),
            vec![Output::CloseAudioCapture { key: k }]
        );
    }
    let mut e = prepared();
    let k = key(&run(&mut e, open(PEER, 2, AudioKind::Microphone)));
    refused(&run(&mut e, shown(k, false)), Refusal::InjectorFailed);
    no_start_or_capture(&run(&mut e, shown(k, true)));
}

#[test]
fn unexpected_wrong_kind_and_failed_device_completions_fail_closed() {
    for kind in KINDS {
        for failure in [
            Failure::Other,
            Failure::Locked,
            Failure::SecureInput,
            Failure::PermissionDenied,
        ] {
            let mut e = prepared();
            let k = incoming(&mut e, 2, kind, false);
            let out = run(&mut e, done(k, kind, Err(failure)));
            refused(
                &out,
                match failure {
                    Failure::Locked | Failure::SecureInput => Refusal::Locked,
                    Failure::PermissionDenied => Refusal::Permission,
                    _ => Refusal::InjectorFailed,
                },
            );
            closed(&out, k, kind);
        }
        let mut e = prepared();
        let k = incoming(&mut e, 2, kind, false);
        let wrong = if kind == AudioKind::Speaker {
            AudioKind::Microphone
        } else {
            AudioKind::Speaker
        };
        let out = run(&mut e, done(k, wrong, Ok(())));
        no_start_or_capture(&out);
        closed(&out, k, kind);
        assert!(out.contains(&if wrong == AudioKind::Microphone {
            Output::CloseAudioCapture { key: k }
        } else {
            Output::CloseAudioPlayback { key: k }
        }));
    }
    let mut e = prepared();
    let k = key(&run(&mut e, open(PEER, 2, AudioKind::Microphone)));
    let out = run(&mut e, done(k, AudioKind::Microphone, Ok(())));
    closed(&out, k, AudioKind::Microphone);
    no_start_or_capture(&out);
}

#[test]
fn total_deadline_covers_indicator_open_and_handshake_callbacks() {
    for stage in 0..3 {
        let mut e = prepared();
        let k = key(&run(&mut e, open(PEER, 2, AudioKind::Microphone)));
        assert!(e.next_deadline().is_some_and(|t| t <= ms(2000)));
        if stage != 0 {
            e.handle(shown(k, true), ms(1900));
        }
        let out = match stage {
            0 => e.handle(shown(k, true), ms(2000)),
            1 => e.handle(done(k, AudioKind::Microphone, Ok(())), ms(2000)),
            _ => e.handle(Input::Tick, ms(2000)),
        };
        refused(&out, Refusal::InjectorFailed);
        no_start_or_capture(&e.handle(shown(k, true), ms(2100)));
        assert!(
            e.handle(done(k, AudioKind::Microphone, Ok(())), ms(2100))
                .contains(&Output::CloseAudioCapture { key: k })
        );
    }
    for kind in KINDS {
        let mut e = prepared();
        let stream = outgoing(&run(&mut e, activity(PEER, kind, true)));
        refused(
            &e.handle(
                control(PEER, ControlMessage::AudioOpened { stream }),
                ms(2000),
            ),
            Refusal::InjectorFailed,
        );
        let mut e = prepared();
        let k = incoming(&mut e, 2, kind, false);
        refused(&e.handle(Input::Tick, ms(2000)), Refusal::InjectorFailed);
        no_start_or_capture(&e.handle(done(k, kind, Ok(())), ms(2001)));
    }
}

#[test]
fn inactive_is_idempotent_and_never_opens_physical_devices() {
    for kind in KINDS {
        for active in [false, true] {
            let mut e = prepared();
            let stream = outgoing(&run(&mut e, activity(PEER, kind, true)));
            assert!(only(run(&mut e, activity(PEER, kind, true))).is_empty());
            if active {
                run(
                    &mut e,
                    control(PEER, ControlMessage::AudioOpened { stream }),
                );
            }
            let out = run(&mut e, activity(PEER, kind, false));
            assert!(
                out.iter()
                    .any(|o| matches!(o, Output::StopAudioStream { .. }))
            );
            assert!(out.contains(&Output::SendControl {
                peer: PEER,
                msg: ControlMessage::AudioClose { stream }
            }));
            assert!(!out.iter().any(|o| matches!(
                o,
                Output::OpenAudioCapture { .. }
                    | Output::OpenAudioPlayback { .. }
                    | Output::CloseAudioCapture { .. }
                    | Output::CloseAudioPlayback { .. }
            )));
            assert!(only(run(&mut e, activity(PEER, kind, false))).is_empty());
            no_start_or_capture(&run(
                &mut e,
                control(PEER, ControlMessage::AudioOpened { stream }),
            ));
            assert!(outgoing(&run(&mut e, activity(PEER, kind, true))).0 > stream.0);
        }
    }
}

fn closures() -> Vec<Input> {
    let mut events = vec![
        Input::Command(Command::Panic),
        grants(&[]),
        feature(PEER, false),
        Input::Link(LinkEvent::Closed {
            peer: PEER,
            error: LinkError::Closed,
        }),
        Input::Session(SessionEvent::WillSleep),
        Input::Session(SessionEvent::Woke),
    ];
    for lock in [LockState::Unlocked, LockState::Locked, LockState::Unknown] {
        for active in [Some(true), Some(false), None] {
            if lock != LockState::Unlocked || active != Some(true) {
                events.push(Input::Session(SessionEvent::State(SessionState {
                    lock,
                    active,
                })));
            }
        }
    }
    events
}
#[test]
fn every_gate_panic_grant_revocation_negotiation_and_link_loss_tears_down() {
    for event in closures() {
        let devices_removed = matches!(
            &event,
            Input::AudioPeer {
                available: false,
                ..
            } | Input::Link(LinkEvent::Closed { .. })
        );
        for kind in KINDS {
            for active in [false, true] {
                let mut e = prepared();
                let k = incoming(&mut e, 2, kind, active);
                let stream = outgoing(&run(&mut e, activity(PEER, kind, true)));
                if active {
                    run(
                        &mut e,
                        control(PEER, ControlMessage::AudioOpened { stream }),
                    );
                }
                let out = run(&mut e, event.clone());
                closed(&out, k, kind);
                assert!(out.iter().any(|o| matches!(o, Output::StopAudioStream { key } if key.stream == stream && key.peer == PEER)));
                run(&mut e, Input::Session(SessionEvent::Woke));
                run(&mut e, unlocked());
                run(&mut e, Input::Command(Command::Rearm));
                connect(&mut e, PEER);
                run(&mut e, grants(&[PEER]));
                let renewed = run(&mut e, activity(PEER, kind, true));
                if devices_removed {
                    outgoing(&renewed);
                } else {
                    assert!(only(renewed).is_empty());
                }
                no_start_or_capture(&run(&mut e, shown(k, true)));
                assert_eq!(
                    only(run(&mut e, done(k, kind, Ok(())))),
                    vec![if kind == AudioKind::Microphone {
                        Output::CloseAudioCapture { key: k }
                    } else {
                        Output::CloseAudioPlayback { key: k }
                    }]
                );
                run(&mut e, activity(PEER, kind, false));
                outgoing(&run(&mut e, activity(PEER, kind, true)));
            }
        }
    }
    // Pending indicator has no physical device to close, but must clear on every teardown.
    for event in closures() {
        let mut e = prepared();
        let k = key(&run(&mut e, open(PEER, 2, AudioKind::Microphone)));
        let out = run(&mut e, event);
        no_start_or_capture(&out);
        assert!(out.iter().any(|o| matches!(o, Output::AudioIndicators { microphones, .. } if !microphones.contains(&k))));
        no_start_or_capture(&run(&mut e, shown(k, true)));
    }
}

#[test]
fn panic_disarms_and_rearm_requires_a_deliberate_activity_edge() {
    let mut e = prepared();
    run(&mut e, Input::Command(Command::Panic));
    refused(
        &run(&mut e, activity(PEER, AudioKind::Microphone, true)),
        Refusal::Locked,
    );
    run(&mut e, unlocked());
    refused(
        &run(&mut e, open(PEER, 2, AudioKind::Microphone)),
        Refusal::Locked,
    );
    run(&mut e, Input::Command(Command::Rearm));
    assert!(only(run(&mut e, activity(PEER, AudioKind::Microphone, true))).is_empty());
    run(&mut e, activity(PEER, AudioKind::Microphone, false));
    outgoing(&run(&mut e, activity(PEER, AudioKind::Microphone, true)));
    key(&run(&mut e, open(PEER, 4, AudioKind::Microphone)));
}

#[test]
fn timed_hotkey_engine_gate_stops_audio() {
    let mut e = prepared();
    let k = incoming(&mut e, 2, AudioKind::Microphone, true);
    run(&mut e, Input::Hotkey(HotkeyEvent::Pressed { at: ms(0) }));
    let out = e.handle(Input::Tick, ms(1000));
    closed(&out, k, AudioKind::Microphone);
    assert!(out.contains(&Output::EngineGate(false)));
    refused(
        &e.handle(open(PEER, 4, AudioKind::Microphone), ms(1001)),
        Refusal::Locked,
    );
}

#[test]
fn reciprocal_and_peer_scoped_streams_cannot_cross_close() {
    let mut a = prepared();
    let mut b = mic_engine(PEER);
    connect(&mut b, LOCAL);
    run(&mut b, grants(&[LOCAL]));
    let sa = outgoing(&run(&mut a, activity(PEER, AudioKind::Microphone, true)));
    let sb = outgoing(&run(&mut b, activity(LOCAL, AudioKind::Microphone, true)));
    assert_ne!(sa, sb);
    let ka = key(&run(&mut a, open(PEER, sb.0, AudioKind::Microphone)));
    let kb = key(&run(&mut b, open(LOCAL, sa.0, AudioKind::Microphone)));
    run(&mut a, shown(ka, true));
    run(&mut b, shown(kb, true));
    run(&mut a, done(ka, AudioKind::Microphone, Ok(())));
    run(&mut b, done(kb, AudioKind::Microphone, Ok(())));
    run(
        &mut a,
        control(PEER, ControlMessage::AudioOpened { stream: sa }),
    );
    run(
        &mut b,
        control(LOCAL, ControlMessage::AudioOpened { stream: sb }),
    );
    let out = run(&mut a, activity(PEER, AudioKind::Microphone, false));
    assert!(!out.contains(&Output::StopAudioStream { key: ka }));
    closed(
        &run(
            &mut b,
            control(LOCAL, ControlMessage::AudioClose { stream: sa }),
        ),
        kb,
        AudioKind::Microphone,
    );
    connect(&mut a, OTHER);
    run(&mut a, grants(&[PEER, OTHER]));
    let kc = key(&run(&mut a, open(OTHER, sb.0, AudioKind::Microphone)));
    run(&mut a, shown(kc, true));
    run(&mut a, done(kc, AudioKind::Microphone, Ok(())));
    let out = run(
        &mut a,
        control(PEER, ControlMessage::AudioClose { stream: sb }),
    );
    closed(&out, ka, AudioKind::Microphone);
    assert!(!out.contains(&Output::StopAudioStream { key: kc }));
}

#[test]
fn reconnect_generations_isolate_old_indicator_and_device_callbacks() {
    for kind in KINDS {
        let mut e = prepared();
        let old = incoming(&mut e, 2, kind, false);
        run(
            &mut e,
            Input::Link(LinkEvent::Closed {
                peer: PEER,
                error: LinkError::Closed,
            }),
        );
        connect(&mut e, PEER);
        // The old wire ID is rejected for the rest of the process (WP-3.0b); a higher one is
        // admitted with a fresh generation.
        refused(&run(&mut e, open(PEER, 2, kind)), Refusal::Permission);
        let new = key(&run(&mut e, open(PEER, 4, kind)));
        assert_ne!(old, new);
        assert_ne!(old.stream, new.stream);
        assert!(new.generation > old.generation);
        assert!(only(run(&mut e, shown(old, true))).is_empty());
        assert!(only(run(&mut e, shown(old, false))).is_empty());
        assert_eq!(
            only(run(&mut e, done(old, kind, Ok(())))),
            vec![if kind == AudioKind::Microphone {
                Output::CloseAudioCapture { key: old }
            } else {
                Output::CloseAudioPlayback { key: old }
            }]
        );
        assert!(only(run(&mut e, done(old, kind, Err(Failure::Other)))).is_empty());
        if kind == AudioKind::Microphone {
            assert_eq!(
                only(run(&mut e, shown(new, true))),
                vec![Output::OpenAudioCapture { key: new }]
            );
        }
        assert!(
            run(&mut e, done(new, kind, Ok(())))
                .iter()
                .any(|o| matches!(o, Output::StartAudioStream { key, .. } if *key == new))
        );
    }
}

#[test]
fn audio_opened_for_incoming_ends_indicator_opening_and_active_admissions() {
    for kind in KINDS {
        // Speaker admission starts in Opening; only microphone has an Indicator phase.
        for phase in 0..3 {
            if kind == AudioKind::Speaker && phase == 0 {
                continue;
            }
            let mut e = prepared();
            let k = key(&run(&mut e, open(PEER, 2, kind)));
            if kind == AudioKind::Microphone && phase > 0 {
                run(&mut e, shown(k, true));
            }
            if phase == 2 {
                run(&mut e, done(k, kind, Ok(())));
            }
            let out = run(
                &mut e,
                control(PEER, ControlMessage::AudioOpened { stream: k.stream }),
            );
            no_start_or_capture(&out);
            assert!(out.contains(&Output::SendControl {
                peer: PEER,
                msg: ControlMessage::AudioClose { stream: k.stream },
            }));
            if phase == 0 {
                assert_eq!(
                    only(out),
                    vec![
                        Output::StopAudioStream { key: k },
                        Output::SendControl {
                            peer: PEER,
                            msg: ControlMessage::AudioClose { stream: k.stream },
                        },
                        Output::AudioIndicators {
                            microphones: vec![],
                            speakers: vec![],
                        },
                    ]
                );
            } else {
                closed(&out, k, kind);
            }
            assert!(only(run(&mut e, shown(k, true))).is_empty());
            assert!(only(run(&mut e, shown(k, false))).is_empty());
            assert_eq!(
                only(run(&mut e, done(k, kind, Ok(())))),
                vec![if kind == AudioKind::Microphone {
                    Output::CloseAudioCapture { key: k }
                } else {
                    Output::CloseAudioPlayback { key: k }
                }]
            );
            assert!(only(run(&mut e, done(k, kind, Err(Failure::Other)))).is_empty());
            assert_eq!(
                only(run(
                    &mut e,
                    control(PEER, ControlMessage::AudioOpened { stream: k.stream })
                )),
                vec![Output::SendControl {
                    peer: PEER,
                    msg: ControlMessage::AudioClose { stream: k.stream },
                }]
            );
            assert!(
                only(run(
                    &mut e,
                    control(PEER, ControlMessage::AudioClose { stream: k.stream })
                ))
                .is_empty()
            );
            // The violation released the kind's admission slot.
            incoming(&mut e, 4, kind, true);
        }
    }
}

#[test]
fn repeated_incoming_open_ends_all_phases_before_refusal_and_rejects_late_callbacks() {
    for kind in KINDS {
        for phase in 0..3 {
            if kind == AudioKind::Speaker && phase == 0 {
                continue;
            }
            // Exact duplicates, changed kind and malformed channels all end the original key.
            for repeat in [
                open(PEER, 2, kind),
                open(
                    PEER,
                    2,
                    if kind == AudioKind::Speaker {
                        AudioKind::Microphone
                    } else {
                        AudioKind::Speaker
                    },
                ),
                control(
                    PEER,
                    ControlMessage::AudioOpen {
                        stream: AudioStreamId(2),
                        kind,
                        channels: 3,
                    },
                ),
            ] {
                let mut e = prepared();
                let k = key(&run(&mut e, open(PEER, 2, kind)));
                if kind == AudioKind::Microphone && phase > 0 {
                    run(&mut e, shown(k, true));
                }
                if phase == 2 {
                    run(&mut e, done(k, kind, Ok(())));
                }
                let out = only(run(&mut e, repeat));
                refused(&out, Refusal::Permission);
                assert_eq!(out.first(), Some(&Output::StopAudioStream { key: k }));
                if phase > 0 {
                    closed(&out, k, kind);
                } else {
                    assert!(!out.iter().any(|o| matches!(
                        o,
                        Output::CloseAudioCapture { .. } | Output::CloseAudioPlayback { .. }
                    )));
                }
                let cleared = out
                    .iter()
                    .position(|o| {
                        matches!(o,
                            Output::AudioIndicators { microphones, speakers }
                                if microphones.is_empty() && speakers.is_empty()
                        )
                    })
                    .unwrap();
                let refusal = out.iter().position(|o| matches!(o,
                    Output::SendControl { msg: ControlMessage::AudioRefused { stream, reason: Refusal::Permission }, .. }
                        if *stream == k.stream
                )).unwrap();
                assert!(cleared < refusal);
                assert!(only(run(&mut e, shown(k, true))).is_empty());
                assert!(only(run(&mut e, shown(k, false))).is_empty());
                assert_eq!(
                    only(run(&mut e, done(k, kind, Ok(())))),
                    vec![if kind == AudioKind::Microphone {
                        Output::CloseAudioCapture { key: k }
                    } else {
                        Output::CloseAudioPlayback { key: k }
                    }]
                );
                assert!(only(run(&mut e, done(k, kind, Err(Failure::Other)))).is_empty());
                assert!(
                    only(run(
                        &mut e,
                        control(PEER, ControlMessage::AudioClose { stream: k.stream })
                    ))
                    .is_empty()
                );
                incoming(&mut e, 4, kind, true);
            }
        }
    }
}

#[test]
fn distinct_oversubscribed_and_wrong_parity_opens_preserve_valid_sessions() {
    for kind in KINDS {
        for phase in 0..3 {
            if kind == AudioKind::Speaker && phase == 0 {
                continue;
            }
            let mut e = prepared();
            let k = key(&run(&mut e, open(PEER, 2, kind)));
            if kind == AudioKind::Microphone && phase > 0 {
                run(&mut e, shown(k, true));
            }
            if phase == 2 {
                run(&mut e, done(k, kind, Ok(())));
            }
            let stream = outgoing(&run(&mut e, activity(PEER, kind, true)));
            for outgoing_active in [false, true] {
                if outgoing_active {
                    run(
                        &mut e,
                        control(PEER, ControlMessage::AudioOpened { stream }),
                    );
                }
                let out = run(&mut e, open(PEER, stream.0, kind));
                refused(&out, Refusal::Permission);
                assert!(
                    !out.iter()
                        .any(|o| matches!(o, Output::StopAudioStream { .. }))
                );
                let out = run(
                    &mut e,
                    open(PEER, if outgoing_active { 6 } else { 4 }, kind),
                );
                refused(&out, Refusal::Busy);
                assert!(!out.iter().any(|o| matches!(
                    o,
                    Output::StopAudioStream { .. } | Output::AudioIndicators { .. }
                )));
            }
            if phase == 0 {
                assert!(run(&mut e, shown(k, true)).contains(&Output::OpenAudioCapture { key: k }));
            }
            if phase < 2 {
                assert!(
                    run(&mut e, done(k, kind, Ok(())))
                        .iter()
                        .any(|o| matches!(o, Output::StartAudioStream { key, .. } if *key == k))
                );
            }
            assert!(
                only(run(
                    &mut e,
                    control(PEER, ControlMessage::AudioOpened { stream })
                ))
                .is_empty()
            );
            let out = run(&mut e, activity(PEER, kind, false));
            assert!(out.iter().any(|o| matches!(o,
                Output::StopAudioStream { key } if key.stream == stream
            )));
            assert!(!out.contains(&Output::StopAudioStream { key: k }));
        }
    }
}

#[test]
fn unknown_and_unnegotiated_virtual_activity_cannot_poison_first_device_demand() {
    for kind in KINDS {
        for peer_up in [false, true] {
            let mut e = mic_engine(LOCAL);
            if peer_up {
                run(&mut e, Input::PeerUp { peer: PEER });
            }
            for active in [true, false, true] {
                assert!(only(run(&mut e, activity(PEER, kind, active))).is_empty());
                assert!(only(run(&mut e, activity(OTHER, kind, active))).is_empty());
            }
            connect(&mut e, PEER);
            outgoing(&run(&mut e, activity(PEER, kind, true)));
            connect(&mut e, OTHER);
            outgoing(&run(&mut e, activity(OTHER, kind, true)));
        }
    }
}

#[test]
fn removed_devices_allow_fresh_demand_without_an_old_inactive_event() {
    for kind in KINDS {
        for link_down in [false, true] {
            for active in [false, true] {
                let mut e = prepared();
                connect(&mut e, OTHER);
                let other = outgoing(&run(&mut e, activity(OTHER, kind, true)));
                let old = outgoing(&run(&mut e, activity(PEER, kind, true)));
                if active {
                    run(
                        &mut e,
                        control(PEER, ControlMessage::AudioOpened { stream: old }),
                    );
                }
                let out = run(
                    &mut e,
                    if link_down {
                        Input::Link(LinkEvent::Closed {
                            peer: PEER,
                            error: LinkError::Closed,
                        })
                    } else {
                        feature(PEER, false)
                    },
                );
                assert!(out.contains(&Output::RemoveAudioPeer { peer: PEER }));
                assert!(out.iter().any(|o| matches!(o, Output::StopAudioStream { key } if key.peer == PEER && key.stream == old)));
                assert!(
                    !out.iter()
                        .any(|o| matches!(o, Output::StopAudioStream { key } if key.peer == OTHER))
                );
                assert!(only(run(&mut e, activity(OTHER, kind, true))).is_empty());
                // A late event from the removed backend must not re-latch its demand.
                assert!(only(run(&mut e, activity(PEER, kind, true))).is_empty());
                if link_down {
                    run(&mut e, Input::PeerUp { peer: PEER });
                    assert!(only(run(&mut e, activity(PEER, kind, true))).is_empty());
                }
                assert!(
                    run(&mut e, feature(PEER, true)).contains(&Output::AddAudioPeer {
                        peer: PEER,
                        name: "display text".into(),
                    })
                );
                let new = outgoing(&run(&mut e, activity(PEER, kind, true)));
                // Neither a device removal nor a real disconnect reuses an ID (WP-3.0b).
                assert!(new.0 > old.0);
                assert!(only(run(&mut e, activity(PEER, kind, true))).is_empty());
                let out = run(
                    &mut e,
                    control(PEER, ControlMessage::AudioOpened { stream: new }),
                );
                assert!(out.iter().any(|o| matches!(o, Output::StartAudioStream { key, kind: k, .. } if key.peer == PEER && key.stream == new && *k == kind)));
                // The other peer's pending request and activity latch survive removal.
                assert!(
                    run(
                        &mut e,
                        control(OTHER, ControlMessage::AudioOpened { stream: other })
                    )
                    .iter()
                    .any(
                        |o| matches!(o, Output::StartAudioStream { key, .. } if key.peer == OTHER)
                    )
                );
            }
        }
    }
}

#[test]
fn duplicate_stale_responses_and_cancelled_late_opens_are_safe() {
    for kind in KINDS {
        let mut e = prepared();
        let s = outgoing(&run(&mut e, activity(PEER, kind, true)));
        run(
            &mut e,
            control(
                PEER,
                ControlMessage::AudioRefused {
                    stream: s,
                    reason: Refusal::Permission,
                },
            ),
        );
        assert!(
            run(
                &mut e,
                control(PEER, ControlMessage::AudioOpened { stream: s })
            )
            .contains(&Output::SendControl {
                peer: PEER,
                msg: ControlMessage::AudioClose { stream: s }
            })
        );
        run(&mut e, activity(PEER, kind, false));
        let next = outgoing(&run(&mut e, activity(PEER, kind, true)));
        assert!(next.0 > s.0);
        run(
            &mut e,
            control(PEER, ControlMessage::AudioOpened { stream: next }),
        );
        assert!(
            only(run(
                &mut e,
                control(PEER, ControlMessage::AudioOpened { stream: next })
            ))
            .is_empty()
        );
        assert!(
            only(run(
                &mut e,
                control(
                    PEER,
                    ControlMessage::AudioClose {
                        stream: AudioStreamId(99)
                    }
                )
            ))
            .is_empty()
        );
        let k = incoming(&mut e, 2, kind, false);
        run(
            &mut e,
            control(PEER, ControlMessage::AudioClose { stream: k.stream }),
        );
        assert_eq!(
            only(run(&mut e, done(k, kind, Ok(())))),
            vec![if kind == AudioKind::Microphone {
                Output::CloseAudioCapture { key: k }
            } else {
                Output::CloseAudioPlayback { key: k }
            }]
        );
        let active = incoming(&mut e, 4, kind, true);
        let out = run(&mut e, done(active, kind, Ok(())));
        closed(&out, active, kind);
        no_start_or_capture(&out);
    }
}

#[test]
fn indicators_are_complete_sorted_and_device_error_scope_is_precise() {
    for kind in KINDS {
        for physical in [false, true] {
            let mut e = prepared();
            connect(&mut e, OTHER);
            run(&mut e, grants(&[PEER, OTHER]));
            let other = key(&run(&mut e, open(OTHER, 2, kind)));
            let peer = key(&run(&mut e, open(PEER, 2, kind)));
            let out = run(&mut e, shown(peer, true));
            if kind == AudioKind::Microphone {
                assert!(out.contains(&Output::OpenAudioCapture { key: peer }));
                run(&mut e, shown(other, true));
            }
            run(&mut e, done(peer, kind, Ok(())));
            run(&mut e, done(other, kind, Ok(())));
            let stream = outgoing(&run(&mut e, activity(PEER, kind, true)));
            run(
                &mut e,
                control(PEER, ControlMessage::AudioOpened { stream }),
            );
            let out = run(
                &mut e,
                Input::Audio(AudioEvent::DeviceError {
                    peer: if physical { None } else { Some(PEER) },
                    kind,
                    error: AudioDeviceError::Unavailable,
                }),
            );
            if physical {
                closed(&out, peer, kind);
                closed(&out, other, kind);
                assert!(
                    !out.iter().any(
                        |o| matches!(o, Output::StopAudioStream { key } if key.stream == stream)
                    )
                );
            } else {
                assert!(!out.contains(&Output::StopAudioStream { key: peer }));
                assert!(!out.contains(&Output::StopAudioStream { key: other }));
            }
        }
    }
    let mut e = prepared();
    connect(&mut e, OTHER);
    run(&mut e, grants(&[PEER, OTHER]));
    let other = key(&run(&mut e, open(OTHER, 2, AudioKind::Microphone)));
    let out = run(&mut e, open(PEER, 2, AudioKind::Microphone));
    let peer = key(&out);
    assert!(out.contains(&Output::AudioIndicators {
        microphones: vec![peer, other],
        speakers: vec![]
    }));
    for (error, reason) in [
        (AudioDeviceError::PermissionDenied, Refusal::Permission),
        (AudioDeviceError::Locked, Refusal::Locked),
        (AudioDeviceError::Failed, Refusal::InjectorFailed),
    ] {
        let mut e = prepared();
        incoming(&mut e, 2, AudioKind::Microphone, true);
        refused(
            &run(
                &mut e,
                Input::Audio(AudioEvent::DeviceError {
                    peer: None,
                    kind: AudioKind::Microphone,
                    error,
                }),
            ),
            reason,
        );
    }
}

#[test]
fn remote_grant_revocation_stops_outgoing_pending_and_active_without_restart() {
    for kind in KINDS {
        for active in [false, true] {
            let mut e = prepared();
            let stream = outgoing(&run(&mut e, activity(PEER, kind, true)));
            if active {
                run(
                    &mut e,
                    control(PEER, ControlMessage::AudioOpened { stream }),
                );
            }
            let out = run(&mut e, control(PEER, ControlMessage::Grants(vec![])));
            assert!(
                out.iter()
                    .any(|o| matches!(o, Output::StopAudioStream { key } if key.stream == stream))
            );
            assert!(only(run(&mut e, activity(PEER, kind, true))).is_empty());
        }
    }
}

#[test]
fn local_grants_remove_only_applicable_kinds() {
    // Outgoing use is authorized by the remote engine; unrelated local grant snapshots
    // do not require granting the remote peer our own physical devices.
    let mut ungranted = mic_engine(LOCAL);
    connect(&mut ungranted, PEER);
    let stream = outgoing(&run(
        &mut ungranted,
        activity(PEER, AudioKind::Microphone, true),
    ));
    assert!(only(run(&mut ungranted, grants(&[]))).is_empty());
    assert!(
        run(
            &mut ungranted,
            control(PEER, ControlMessage::AudioOpened { stream })
        )
        .iter()
        .any(|o| matches!(o, Output::StartAudioStream { .. }))
    );
    let mut e = prepared();
    let speaker = incoming(&mut e, 2, AudioKind::Speaker, true);
    let mic = incoming(&mut e, 4, AudioKind::Microphone, true);
    let out = run(
        &mut e,
        Input::Grants(BTreeMap::from([(
            PEER,
            BTreeSet::from([Capability::AudioSpeaker]),
        )])),
    );
    closed(&out, mic, AudioKind::Microphone);
    assert!(!out.contains(&Output::StopAudioStream { key: speaker }));
    assert!(out.contains(&Output::AudioIndicators {
        microphones: vec![],
        speakers: vec![speaker]
    }));
}

// ---- WP-3.6b: identity counters, connection replacement, stream failure, microphones ----------

fn phases(kind: AudioKind) -> &'static [u8] {
    // 0: pending indicator (microphone only), 1: opening the device, 2: active.
    if kind == AudioKind::Microphone {
        &[0, 1, 2]
    } else {
        &[1, 2]
    }
}
fn incoming_at(e: &mut Engine, id: u16, kind: AudioKind, phase: u8) -> AudioKey {
    let k = key(&run(e, open(PEER, id, kind)));
    if kind == AudioKind::Microphone && phase > 0 {
        run(e, shown(k, true));
    }
    if phase == 2 {
        run(e, done(k, kind, Ok(())));
    }
    k
}
fn stops(out: &[Output]) -> Vec<AudioKey> {
    out.iter()
        .filter_map(|o| match o {
            Output::StopAudioStream { key } => Some(*key),
            _ => None,
        })
        .collect()
}
fn sent_close(out: &[Output], stream: AudioStreamId) -> bool {
    out.contains(&Output::SendControl {
        peer: PEER,
        msg: ControlMessage::AudioClose { stream },
    })
}
fn device_close(key: AudioKey, kind: AudioKind) -> Output {
    if kind == AudioKind::Microphone {
        Output::CloseAudioCapture { key }
    } else {
        Output::CloseAudioPlayback { key }
    }
}
fn started_key(out: &[Output]) -> AudioKey {
    out.iter()
        .find_map(|o| match o {
            Output::StartAudioStream { key, .. } => Some(*key),
            _ => None,
        })
        .unwrap()
}
fn no_indicator_or_capture(out: &[Output]) {
    assert!(!out.iter().any(|o| matches!(
        o,
        Output::AudioIndicators { .. }
            | Output::OpenAudioCapture { .. }
            | Output::CloseAudioCapture { .. }
            | Output::StartAudioStream { .. }
            | Output::Notice(Notice::MicInUseBy(_))
    )));
}
fn no_audio_output(out: &[Output]) {
    assert!(
        !out.iter().any(audio_output),
        "unexpected audio output: {out:?}"
    );
}
fn disconnect(e: &mut Engine, peer: NodeId) -> Vec<Output> {
    run(
        e,
        Input::Link(LinkEvent::Closed {
            peer,
            error: LinkError::Closed,
        }),
    )
}

#[test]
fn production_refuses_microphones_in_both_directions_without_any_device_or_indicator() {
    let mut e = speaker_only();
    // A valid incoming microphone ID is consumed and refused as unsupported (InjectorFailed on
    // the wire), with no indicator, no "in use" notice, no capture open and no stream.
    let out = run(&mut e, open(PEER, 2, AudioKind::Microphone));
    refused(&out, Refusal::InjectorFailed);
    no_indicator_or_capture(&out);
    assert!(out.contains(&Output::SendControl {
        peer: PEER,
        msg: ControlMessage::AudioRefused {
            stream: AudioStreamId(2),
            reason: Refusal::InjectorFailed,
        },
    }));
    assert!(stops(&out).is_empty());
    // The ID was consumed: a replay is a replay, the next valid one is refused as unsupported.
    refused(
        &run(&mut e, open(PEER, 2, AudioKind::Microphone)),
        Refusal::Permission,
    );
    let out = run(&mut e, open(PEER, 4, AudioKind::Microphone));
    refused(&out, Refusal::InjectorFailed);
    no_indicator_or_capture(&out);
    // Invalid requests keep their own refusal and consume nothing they should not.
    refused(
        &run(&mut e, open(PEER, 5, AudioKind::Microphone)),
        Refusal::Permission,
    );
    refused(
        &run(
            &mut e,
            control(
                PEER,
                ControlMessage::AudioOpen {
                    stream: AudioStreamId(6),
                    kind: AudioKind::Microphone,
                    channels: 2,
                },
            ),
        ),
        Refusal::Permission,
    );
    // Without a grant the peer learns nothing about support.
    run(&mut e, grants(&[]));
    refused(
        &run(&mut e, open(PEER, 8, AudioKind::Microphone)),
        Refusal::Permission,
    );
    run(&mut e, grants(&[PEER]));
    // Speakers are unaffected and still show their usage indicator.
    let speaker = key(&run(&mut e, open(PEER, 10, AudioKind::Speaker)));
    assert!(
        run(&mut e, done(speaker, AudioKind::Speaker, Ok(())))
            .iter()
            .any(|o| matches!(o, Output::StartAudioStream { key, .. } if *key == speaker))
    );

    // Outgoing demand sends no open, consumes no ID and reports the refusal once per edge.
    let mut e = speaker_only();
    let out = run(&mut e, activity(PEER, AudioKind::Microphone, true));
    assert!(!out.iter().any(|o| matches!(
        o,
        Output::SendControl {
            msg: ControlMessage::AudioOpen { .. },
            ..
        }
    )));
    refused(&out, Refusal::InjectorFailed);
    // Not a single indicator, in-use notice, capture or stream output on the first edge...
    no_indicator_or_capture(&out);
    assert!(only(run(&mut e, activity(PEER, AudioKind::Microphone, true))).is_empty());
    assert!(only(run(&mut e, activity(PEER, AudioKind::Microphone, false))).is_empty());
    let out = run(&mut e, activity(PEER, AudioKind::Microphone, true));
    refused(&out, Refusal::InjectorFailed);
    // ...nor on the next one.
    no_indicator_or_capture(&out);
    assert!(!out.iter().any(|o| matches!(
        o,
        Output::SendControl { .. } | Output::StartAudioStream { .. }
    )));
    assert_eq!(
        outgoing(&run(&mut e, activity(PEER, AudioKind::Speaker, true))),
        AudioStreamId(1)
    );
}

#[test]
fn microphone_support_off_ends_microphone_sessions_keeps_latches_and_speakers() {
    for phase in phases(AudioKind::Microphone) {
        for outgoing_active in [false, true] {
            let mut e = prepared();
            let mic = incoming_at(&mut e, 2, AudioKind::Microphone, *phase);
            let speaker = incoming_at(&mut e, 4, AudioKind::Speaker, 2);
            let mic_out = outgoing(&run(&mut e, activity(PEER, AudioKind::Microphone, true)));
            if outgoing_active {
                run(
                    &mut e,
                    control(PEER, ControlMessage::AudioOpened { stream: mic_out }),
                );
            }
            let out = run(&mut e, microphones(false));
            no_start_or_capture(&out);
            assert!(stops(&out).contains(&mic));
            assert!(
                stops(&out)
                    .iter()
                    .any(|k| k.stream == mic_out && k.peer == PEER)
            );
            assert!(!stops(&out).contains(&speaker));
            if *phase == 0 {
                assert!(!out.contains(&Output::CloseAudioCapture { key: mic }));
            } else {
                closed(&out, mic, AudioKind::Microphone);
            }
            assert!(out.iter().any(|o| matches!(
                o,
                Output::Notice(Notice::AudioRefused {
                    kind: AudioKind::Microphone,
                    reason: Refusal::InjectorFailed,
                    ..
                })
            )));
            assert!(sent_close(&out, mic_out));
            assert!(out.contains(&Output::AudioIndicators {
                microphones: vec![],
                speakers: vec![speaker],
            }));
            // The demand latch survives: no restart until the app goes inactive, then active.
            assert!(only(run(&mut e, activity(PEER, AudioKind::Microphone, true))).is_empty());
            // Stale callbacks for the ended key close its handle and nothing else.
            assert!(only(run(&mut e, shown(mic, true))).is_empty());
            assert_eq!(
                only(run(&mut e, done(mic, AudioKind::Microphone, Ok(())))),
                vec![Output::CloseAudioCapture { key: mic }]
            );
            // Further microphone requests are unsupported; speakers still admit.
            refused(
                &run(&mut e, open(PEER, 6, AudioKind::Microphone)),
                Refusal::InjectorFailed,
            );
            // The speaker session survived the whole time and ends only on its own close.
            let out = run(
                &mut e,
                control(
                    PEER,
                    ControlMessage::AudioClose {
                        stream: speaker.stream,
                    },
                ),
            );
            assert_eq!(stops(&out), vec![speaker]);
            incoming(&mut e, 8, AudioKind::Speaker, true);
            run(&mut e, activity(PEER, AudioKind::Microphone, false));
            let out = run(&mut e, activity(PEER, AudioKind::Microphone, true));
            refused(&out, Refusal::InjectorFailed);
            no_indicator_or_capture(&out);
            assert!(!out.iter().any(|o| matches!(o, Output::SendControl { .. })));
            // Enabling support again restarts nothing by itself, but allows deliberate new use.
            assert!(only(run(&mut e, microphones(true))).is_empty());
            assert!(only(run(&mut e, activity(PEER, AudioKind::Microphone, true))).is_empty());
            run(&mut e, activity(PEER, AudioKind::Microphone, false));
            assert!(
                outgoing(&run(&mut e, activity(PEER, AudioKind::Microphone, true))).0 > mic_out.0
            );
            incoming(&mut e, 10, AudioKind::Microphone, true);
        }
    }
}

#[test]
fn replacement_ends_every_session_but_keeps_availability_counters_and_demand() {
    for kind in KINDS {
        for phase in phases(kind) {
            for outgoing_active in [false, true] {
                let mut e = prepared();
                connect(&mut e, OTHER);
                run(&mut e, grants(&[PEER, OTHER]));
                let bystander = key(&run(&mut e, open(OTHER, 2, AudioKind::Speaker)));
                let old = incoming_at(&mut e, 2, kind, *phase);
                let stream = outgoing(&run(&mut e, activity(PEER, kind, true)));
                if outgoing_active {
                    run(
                        &mut e,
                        control(PEER, ControlMessage::AudioOpened { stream }),
                    );
                }
                let out = run(&mut e, Input::AudioConnectionReplaced { peer: PEER });
                let stopped = stops(&out);
                assert_eq!(stopped.len(), 2, "{out:?}");
                assert!(stopped.contains(&old));
                let outgoing_key = *stopped.iter().find(|k| k.stream == stream).unwrap();
                assert!(!stopped.contains(&bystander));
                if *phase == 0 {
                    // No physical capture was ever opened for a pending indicator.
                    assert!(
                        !out.iter()
                            .any(|o| matches!(o, Output::CloseAudioCapture { .. }))
                    );
                } else {
                    closed(&out, old, kind);
                }
                // The final indicator state lists only the bystander peer's usage.
                let last = out.iter().rev().find_map(|o| match o {
                    Output::AudioIndicators {
                        microphones,
                        speakers,
                    } => Some((microphones.clone(), speakers.clone())),
                    _ => None,
                });
                assert_eq!(last, Some((vec![], vec![bystander])));
                assert!(sent_close(&out, old.stream));
                assert!(sent_close(&out, stream));
                // Silent: no refusal notice, no availability change, nothing starts.
                assert!(!out.iter().any(|o| matches!(
                    o,
                    Output::Notice(Notice::AudioRefused { .. })
                        | Output::AddAudioPeer { .. }
                        | Output::RemoveAudioPeer { .. }
                )));
                no_start_or_capture(&out);
                // Callbacks and responses of the replaced connection cannot restart anything.
                assert!(only(run(&mut e, shown(old, true))).is_empty());
                assert_eq!(
                    only(run(&mut e, done(old, kind, Ok(())))),
                    vec![device_close(old, kind)]
                );
                no_start_or_capture(&run(
                    &mut e,
                    control(PEER, ControlMessage::AudioOpened { stream }),
                ));
                // Availability is untouched; demand and identity counters are retained.
                assert!(only(run(&mut e, feature(PEER, true))).is_empty());
                assert!(only(run(&mut e, activity(PEER, kind, true))).is_empty());
                refused(&run(&mut e, open(PEER, 2, kind)), Refusal::Permission);
                let fresh = key(&run(&mut e, open(PEER, 4, kind)));
                assert!(fresh.generation > old.generation);
                assert!(fresh.generation > outgoing_key.generation);
                assert!(fresh.generation > bystander.generation);
                // The bystander peer's session is still live and ends only on its own events.
                let out = run(
                    &mut e,
                    control(
                        OTHER,
                        ControlMessage::AudioClose {
                            stream: bystander.stream,
                        },
                    ),
                );
                assert_eq!(stops(&out), vec![bystander]);
                // After an inactive/active edge, the next ID continues past the replaced one.
                run(&mut e, activity(PEER, kind, false));
                assert_eq!(
                    outgoing(&run(&mut e, activity(PEER, kind, true))),
                    AudioStreamId(stream.0 + 2)
                );
            }
        }
    }
    // Nothing to end: silent, including for unknown peers.
    let mut e = prepared();
    for peer in [PEER, OTHER] {
        assert!(run(&mut e, Input::AudioConnectionReplaced { peer }).is_empty());
    }
}

#[test]
fn replacement_during_handshake_opening_and_active_never_restarts_without_a_new_edge() {
    for kind in KINDS {
        let mut e = prepared();
        let stream = outgoing(&run(&mut e, activity(PEER, kind, true)));
        // Handshake: replaced before the peer answered; the late answer is refused.
        let out = run(&mut e, Input::AudioConnectionReplaced { peer: PEER });
        assert_eq!(stops(&out).len(), 1);
        assert!(sent_close(&out, stream));
        no_start_or_capture(&run(
            &mut e,
            control(PEER, ControlMessage::AudioOpened { stream }),
        ));
        // Repeated active after replacement does not restart; inactive then active does.
        assert!(only(run(&mut e, activity(PEER, kind, true))).is_empty());
        run(&mut e, activity(PEER, kind, false));
        let next = outgoing(&run(&mut e, activity(PEER, kind, true)));
        assert_eq!(next.0, stream.0 + 2);
        let out = run(
            &mut e,
            control(PEER, ControlMessage::AudioOpened { stream: next }),
        );
        let active = started_key(&out);
        // Active: replaced again; the second replacement is idempotent.
        let out = run(&mut e, Input::AudioConnectionReplaced { peer: PEER });
        assert_eq!(stops(&out), vec![active]);
        assert!(only(run(&mut e, Input::AudioConnectionReplaced { peer: PEER })).is_empty());
        assert!(only(run(&mut e, activity(PEER, kind, true))).is_empty());
    }
}

#[test]
fn real_disconnect_reconnect_uses_larger_ids_and_stale_messages_cannot_touch_new_keys() {
    for kind in KINDS {
        let mut e = prepared();
        let old_in = incoming(&mut e, 2, kind, true);
        let old_out = outgoing(&run(&mut e, activity(PEER, kind, true)));
        let out = disconnect(&mut e, PEER);
        assert!(out.contains(&Output::RemoveAudioPeer { peer: PEER }));
        closed(&out, old_in, kind);
        assert!(stops(&out).iter().any(|k| k.stream == old_out));
        // Availability needs a new link: an AudioPeer with no PeerUp is not trusted, and demand
        // from a removed device cannot latch.
        assert!(only(run(&mut e, feature(PEER, true))).is_empty());
        assert!(only(run(&mut e, activity(PEER, kind, true))).is_empty());
        refused(&run(&mut e, open(PEER, 4, kind)), Refusal::Permission);
        run(&mut e, Input::PeerUp { peer: PEER });
        assert!(
            run(&mut e, feature(PEER, true)).contains(&Output::AddAudioPeer {
                peer: PEER,
                name: "display text".into(),
            })
        );
        // Outgoing IDs continue upward; the old ID is never allocated again.
        let new_out = outgoing(&run(&mut e, activity(PEER, kind, true)));
        assert!(new_out.0 > old_out.0);
        // Incoming: the old ID and every lower one are replays; a higher one is admitted.
        refused(&run(&mut e, open(PEER, 2, kind)), Refusal::Permission);
        // (ID 4 above arrived while disconnected, so it was neither consumed nor admitted.)
        let new_in = key(&run(&mut e, open(PEER, 4, kind)));
        assert!(new_in.generation > old_in.generation);
        assert_ne!(new_in.stream, old_in.stream);
        // Stale controls and completions of the old streams cannot affect the new keys.
        let out = run(
            &mut e,
            control(PEER, ControlMessage::AudioOpened { stream: old_out }),
        );
        no_start_or_capture(&out);
        assert!(stops(&out).is_empty());
        for msg in [
            ControlMessage::AudioClose { stream: old_out },
            ControlMessage::AudioClose {
                stream: old_in.stream,
            },
            ControlMessage::AudioRefused {
                stream: old_out,
                reason: Refusal::Permission,
            },
        ] {
            assert!(stops(&run(&mut e, control(PEER, msg))).is_empty());
        }
        assert!(only(run(&mut e, shown(old_in, true))).is_empty());
        assert!(only(run(&mut e, shown(old_in, false))).is_empty());
        assert_eq!(
            only(run(&mut e, done(old_in, kind, Ok(())))),
            vec![device_close(old_in, kind)]
        );
        assert!(only(run(&mut e, done(old_in, kind, Err(Failure::Other)))).is_empty());
        assert!(only(run(&mut e, Input::AudioStreamFailed { key: old_in })).is_empty());
        // The new sessions complete normally.
        if kind == AudioKind::Microphone {
            assert_eq!(
                only(run(&mut e, shown(new_in, true))),
                vec![Output::OpenAudioCapture { key: new_in }]
            );
        }
        assert!(
            run(&mut e, done(new_in, kind, Ok(())))
                .iter()
                .any(|o| matches!(o, Output::StartAudioStream { key, .. } if *key == new_in))
        );
        assert!(
            run(
                &mut e,
                control(PEER, ControlMessage::AudioOpened { stream: new_out })
            )
            .iter()
            .any(|o| matches!(o, Output::StartAudioStream { key, .. } if key.stream == new_out))
        );
    }
}

#[test]
fn stream_ids_never_wrap_across_reconnects_and_replacements() {
    for local in [LOCAL, OTHER] {
        let first = if local < PEER { 1 } else { 2 };
        let last = if local < PEER { 65_535 } else { 65_534 };
        let mut e = engine(local);
        connect(&mut e, PEER);
        run(&mut e, grants(&[PEER]));
        let mut previous = 0u16;
        let mut rounds = 0u32;
        loop {
            let out = run(&mut e, activity(PEER, AudioKind::Speaker, true));
            let Some(stream) = out.iter().find_map(|o| match o {
                Output::SendControl {
                    msg: ControlMessage::AudioOpen { stream, .. },
                    ..
                } => Some(*stream),
                _ => None,
            }) else {
                // Exhausted: refused as Busy, nothing sent.
                assert!(out.iter().any(|o| matches!(
                    o,
                    Output::Notice(Notice::AudioRefused {
                        reason: Refusal::Busy,
                        ..
                    })
                )));
                assert!(!out.iter().any(|o| matches!(o, Output::SendControl { .. })));
                break;
            };
            // Strictly ascending in this node's parity namespace, with no ID skipped or reused.
            assert_eq!(stream.0, if previous == 0 { first } else { previous + 2 });
            previous = stream.0;
            rounds += 1;
            if rounds.is_multiple_of(1_000) {
                // A real disconnect and reconnect keep the counters.
                disconnect(&mut e, PEER);
                run(&mut e, Input::PeerUp { peer: PEER });
                run(&mut e, feature(PEER, true));
            } else if rounds.is_multiple_of(7) {
                // A silent replacement while the open is pending keeps them too.
                run(&mut e, Input::AudioConnectionReplaced { peer: PEER });
                run(&mut e, activity(PEER, AudioKind::Speaker, false));
            } else {
                run(&mut e, activity(PEER, AudioKind::Speaker, false));
            }
        }
        assert_eq!(previous, last);
        // Exhaustion is permanent: further demand, after a reconnect or replacement, is
        // still refused and never wraps back to a used ID.
        for reconnect in [false, true] {
            if reconnect {
                disconnect(&mut e, PEER);
                run(&mut e, Input::PeerUp { peer: PEER });
                run(&mut e, feature(PEER, true));
            } else {
                run(&mut e, Input::AudioConnectionReplaced { peer: PEER });
            }
            run(&mut e, activity(PEER, AudioKind::Speaker, false));
            let out = run(&mut e, activity(PEER, AudioKind::Speaker, true));
            assert!(!out.iter().any(|o| matches!(o, Output::SendControl { .. })));
            refused(&out, Refusal::Busy);
        }
    }
    // The remote namespace: the highest ID is admitted once; no ID, however low, is ever
    // admitted again after reconnecting.
    let mut e = prepared();
    let highest = incoming(&mut e, 65_534, AudioKind::Speaker, true);
    disconnect(&mut e, PEER);
    connect(&mut e, PEER);
    for id in [2, 4, 65_532, 65_534] {
        refused(
            &run(&mut e, open(PEER, id, AudioKind::Speaker)),
            Refusal::Permission,
        );
    }
    assert_eq!(
        only(run(&mut e, done(highest, AudioKind::Speaker, Ok(())))),
        vec![Output::CloseAudioPlayback { key: highest }]
    );
}

#[test]
fn stream_failure_ends_only_the_matching_admission_with_injector_failed() {
    for kind in KINDS {
        for phase in phases(kind) {
            let mut e = prepared();
            let k = incoming_at(&mut e, 2, kind, *phase);
            let pending = outgoing(&run(&mut e, activity(PEER, kind, true)));
            // Wrong generation, peer or stream: no effect at all.
            for stale in [
                AudioKey {
                    generation: k.generation + 1,
                    ..k
                },
                AudioKey {
                    generation: k.generation - 1,
                    ..k
                },
                AudioKey { peer: OTHER, ..k },
                AudioKey {
                    stream: AudioStreamId(8),
                    ..k
                },
            ] {
                no_audio_output(&run(&mut e, Input::AudioStreamFailed { key: stale }));
            }
            let out = run(&mut e, Input::AudioStreamFailed { key: k });
            assert_eq!(stops(&out), vec![k], "{out:?}");
            assert!(out.iter().any(|o| matches!(o,
                Output::Notice(Notice::AudioRefused { peer: PEER, kind: nk, reason: Refusal::InjectorFailed })
                    if *nk == kind)));
            if *phase == 0 {
                assert!(
                    !out.iter()
                        .any(|o| matches!(o, Output::CloseAudioCapture { .. }))
                );
            } else {
                closed(&out, k, kind);
            }
            // Admission that never became active is refused on the wire; an active one is closed.
            if *phase == 2 {
                assert!(sent_close(&out, k.stream));
            } else {
                assert!(out.contains(&Output::SendControl {
                    peer: PEER,
                    msg: ControlMessage::AudioRefused {
                        stream: k.stream,
                        reason: Refusal::InjectorFailed,
                    },
                }));
            }
            no_start_or_capture(&out);
            // Stale failures are idempotent, and late callbacks of the ended key stay safe.
            no_audio_output(&run(&mut e, Input::AudioStreamFailed { key: k }));
            assert!(only(run(&mut e, shown(k, true))).is_empty());
            assert_eq!(
                only(run(&mut e, done(k, kind, Ok(())))),
                vec![device_close(k, kind)]
            );
            // The unrelated outgoing request is untouched and still completes.
            assert!(
                run(
                    &mut e,
                    control(PEER, ControlMessage::AudioOpened { stream: pending })
                )
                .iter()
                .any(
                    |o| matches!(o, Output::StartAudioStream { key, .. } if key.stream == pending)
                )
            );
            // The slot is free again for a deliberate new request with a larger ID.
            incoming(&mut e, 4, kind, true);
        }
        // An active outgoing session fails the same way and keeps its demand latch.
        let mut e = prepared();
        let stream = outgoing(&run(&mut e, activity(PEER, kind, true)));
        let active = started_key(&run(
            &mut e,
            control(PEER, ControlMessage::AudioOpened { stream }),
        ));
        let out = run(&mut e, Input::AudioStreamFailed { key: active });
        assert_eq!(stops(&out), vec![active]);
        assert!(sent_close(&out, stream));
        assert!(out.iter().any(|o| matches!(
            o,
            Output::Notice(Notice::AudioRefused {
                reason: Refusal::InjectorFailed,
                ..
            })
        )));
        assert!(!out.iter().any(|o| matches!(
            o,
            Output::CloseAudioCapture { .. }
                | Output::CloseAudioPlayback { .. }
                | Output::AudioIndicators { .. }
        )));
        no_audio_output(&run(&mut e, Input::AudioStreamFailed { key: active }));
        assert!(only(run(&mut e, activity(PEER, kind, true))).is_empty());
        // A failure of the old key cannot end the next session of the same peer and kind.
        run(&mut e, activity(PEER, kind, false));
        let next = outgoing(&run(&mut e, activity(PEER, kind, true)));
        let next_key = started_key(&run(
            &mut e,
            control(PEER, ControlMessage::AudioOpened { stream: next }),
        ));
        assert!(next_key.generation > active.generation);
        no_audio_output(&run(&mut e, Input::AudioStreamFailed { key: active }));
        assert_eq!(
            stops(&run(&mut e, Input::AudioStreamFailed { key: next_key })),
            vec![next_key]
        );
    }
}

#[test]
fn replay_and_hello_refresh_events_do_not_disturb_the_engine() {
    // The engine consumes only the agent's translation of a refresh (AudioConnectionReplaced);
    // the raw transport event is ignored here (WP-3.6 handles it).
    let mut e = prepared();
    let k = incoming(&mut e, 2, AudioKind::Speaker, true);
    let out = run(
        &mut e,
        Input::Link(LinkEvent::HelloRefresh {
            peer: PEER,
            hello: crosspane_protocol::msg::Hello {
                minor: 0,
                name: "peer".into(),
                features: vec!["audio".into()],
                displays: Vec::new(),
            },
        }),
    );
    no_audio_output(&out);
    assert!(!stops(&out).contains(&k));
}
