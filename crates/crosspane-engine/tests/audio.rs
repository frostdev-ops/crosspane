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
fn prepared() -> Engine {
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
            let mut e = engine(local);
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
        let mut e = engine(LOCAL);
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
    let mut b = engine(PEER);
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
        let new = key(&run(&mut e, open(PEER, 2, kind)));
        assert_ne!(old, new);
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
            let mut e = engine(LOCAL);
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
                if !link_down {
                    assert!(new.0 > old.0);
                }
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
    let mut ungranted = engine(LOCAL);
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
