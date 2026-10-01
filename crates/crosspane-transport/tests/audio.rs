//! Synthetic encoded packets only; all sockets belong to these loopback tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use crosspane_media::tiles::TileEncoder;
use crosspane_media::wire::FrameHeader;
use crosspane_protocol::audio::{AudioPacket, AudioStreamId, encode_audio};
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::{Capability, ControlMessage, Hello, PointerMessage, Refusal};
use crosspane_protocol::wire::encode_control;
use crosspane_types::audio::AudioKind;
use crosspane_types::geom::PointDevice;
use crosspane_types::id::{DisplayId, SessionId};
use tokio::time::timeout;

fn advertisement(name: &str, audio: bool) -> Hello {
    let mut hello = hello(name);
    if audio {
        hello.features.push("audio".into());
    }
    hello
}
fn packet(len: usize) -> AudioPacket {
    AudioPacket {
        stream: AudioStreamId(7),
        seq: 19,
        sample_time: 48_000,
        opus: vec![0x5a; len],
    }
}
fn pointer() -> PointerMessage {
    PointerMessage {
        session: SessionId(1),
        seq: 3,
        display: DisplayId(1),
        position: PointDevice::new(2.0, 4.0),
    }
}
fn frame(audio: bool) -> Vec<u8> {
    let mut frame = Vec::new();
    encode_control(
        &ControlMessage::Hello(advertisement("raw", audio)),
        &mut frame,
    )
    .unwrap();
    frame
}
async fn pair_with_audio(local: bool, remote: bool) -> (Node, Node) {
    let ia = identity();
    let ib = identity();
    let mut a = node_with_hello(ia.clone(), &[&ib], advertisement("a", local));
    let mut b = node_with_hello(ib, &[&ia], advertisement("b", remote));
    a.transport.connect(b.addr()).await.unwrap();
    a.expect_hello(b.id, "b").await;
    b.expect_hello(a.id, "a").await;
    (a, b)
}
async fn expect_mixed(
    node: &mut Node,
    peer: crosspane_types::id::NodeId,
    audio: Option<&AudioPacket>,
) {
    let mut seen = [false; 4];
    seen[0] = audio.is_none();
    while !seen.iter().all(|v| *v) {
        match node.next().await {
            event @ LinkEvent::Audio { .. } => {
                // Logging an event must never disclose the encoded samples (0x5a bytes here).
                assert!(!format!("{event:?}").contains("90, 90"));
                let LinkEvent::Audio {
                    peer: from,
                    packet: got,
                } = event
                else {
                    unreachable!()
                };
                assert_eq!(from, peer);
                assert_eq!(&got, audio.unwrap());
                assert!(!seen[0]);
                seen[0] = true;
            }
            LinkEvent::Motion { peer: from, msg } => {
                assert_eq!(from, peer);
                assert_eq!(msg, pointer());
                assert!(!seen[1]);
                seen[1] = true;
            }
            LinkEvent::Control { peer: from, msg } => {
                assert_eq!(from, peer);
                assert_eq!(msg, ControlMessage::Ping { t0: 99 });
                assert!(!seen[2]);
                seen[2] = true;
            }
            LinkEvent::Input { peer: from, msg } => {
                assert_eq!(from, peer);
                assert_eq!(msg, key(1));
                assert!(!seen[3]);
                seen[3] = true;
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
#[tokio::test]
async fn negotiated_audio_interleaves_with_pointer_control_and_input_both_ways() {
    let (mut a, mut b) = pair_with_audio(true, true).await;
    let packets = [packet(1), packet(400)];
    for (source, target, data) in [(&a, &b, &packets[0]), (&b, &a, &packets[1])] {
        let mut link = source.transport.link(target.id).unwrap();
        link.send_motion(&pointer()).unwrap();
        link.send_audio(data).unwrap();
        link.send_control(&ControlMessage::Ping { t0: 99 }).unwrap();
        link.send_input(&key(1)).unwrap();
    }
    let ai = a.id;
    let bi = b.id;
    tokio::join!(
        expect_mixed(&mut a, bi, Some(&packets[1])),
        expect_mixed(&mut b, ai, Some(&packets[0]))
    );
}
#[tokio::test]
async fn either_missing_feature_refuses_audio_and_preserves_old_channels() {
    for (local, remote) in [(false, true), (true, false), (false, false)] {
        let (mut a, mut b) = pair_with_audio(local, remote).await;
        for (source, target) in [(&a, &b), (&b, &a)] {
            let mut link = source.transport.link(target.id).unwrap();
            assert_eq!(
                link.send_audio(&packet(1)),
                Err(LinkError::Invalid("audio is unavailable"))
            );
            link.send_motion(&pointer()).unwrap();
            link.send_control(&ControlMessage::Ping { t0: 99 }).unwrap();
            link.send_input(&key(1)).unwrap();
        }
        let ai = a.id;
        let bi = b.id;
        tokio::join!(
            expect_mixed(&mut a, bi, None),
            expect_mixed(&mut b, ai, None)
        );
    }
}
#[tokio::test]
async fn invalid_packets_fail_send_without_closing() {
    let (a, mut b) = pair_with_audio(true, true).await;
    let mut link = a.transport.link(b.id).unwrap();
    let mut zero = packet(1);
    zero.stream = AudioStreamId(0);
    for bad in [zero, packet(0), packet(401)] {
        assert!(matches!(link.send_audio(&bad), Err(LinkError::Invalid(_))));
    }
    link.send_audio(&packet(1)).unwrap();
    assert!(matches!(b.next().await, LinkEvent::Audio { .. }));
}
#[tokio::test]
async fn pre_hello_send_and_unnegotiated_receive_are_refused() {
    for (local_audio, peer_audio) in [(true, false), (false, true)] {
        let ia = identity();
        let ir = identity();
        let mut a = node_with_hello(ia.clone(), &[&ir], advertisement("a", local_audio));
        let raw = Raw::new();
        let conn = raw.connect(&ir, &ia, a.addr()).await;
        wait_until("link visible before Hello", || {
            a.transport.link(ir.node()).is_some()
        })
        .await;
        let mut link = a.transport.link(ir.node()).unwrap();
        assert_eq!(
            link.send_audio(&packet(1)),
            Err(LinkError::Invalid("audio is unavailable"))
        );
        let _control = open_stream(&conn, 1, &frame(peer_audio)).await;
        a.expect_hello(ir.node(), "raw").await;
        conn.send_datagram(encode_audio(&packet(1)).unwrap().into())
            .unwrap();
        a.expect_closed(ir.node(), LinkError::Invalid("audio is unavailable"))
            .await;
        assert_eq!(closed_by_peer(&conn).await, (1, "protocol error".into()));
        assert_eq!(link.send_audio(&packet(1)), Err(LinkError::Closed));
    }
}
#[tokio::test]
async fn malformed_audio_and_unknown_kind_close_without_delivery() {
    let good = encode_audio(&packet(1)).unwrap();
    let mut cases = Vec::new();
    for (offset, value) in [(0, 0), (2, 1), (3, 1), (4, 0), (4, 255), (8, 0)] {
        let mut bad = good.clone();
        bad[offset] = value;
        cases.push(bad);
    }
    cases.push(good[..7].to_vec());
    cases.push(good[..good.len() - 1].to_vec());
    let mut extra = good.clone();
    extra.push(0);
    cases.push(extra);
    let mut unknown = good;
    unknown[1] = 0xff;
    cases.push(unknown);
    for bad in cases {
        let ia = identity();
        let ir = identity();
        let mut a = node_with_hello(ia.clone(), &[&ir], advertisement("a", true));
        let raw = Raw::new();
        let conn = raw.connect(&ir, &ia, a.addr()).await;
        let _control = open_stream(&conn, 1, &frame(true)).await;
        a.expect_hello(ir.node(), "raw").await;
        let reason = if bad.len() < 8 || bad[1] == 0xff {
            "malformed pointer datagram"
        } else {
            "malformed audio datagram"
        };
        conn.send_datagram(bad.into()).unwrap();
        a.expect_closed(ir.node(), LinkError::Invalid(reason)).await;
        assert_eq!(closed_by_peer(&conn).await, (1, "protocol error".into()));
    }
}
#[tokio::test]
async fn reconnect_cannot_reuse_audio_admission_or_old_handle() {
    let ia = identity();
    let ir = identity();
    let mut a = node_with_hello(ia.clone(), &[&ir], advertisement("a", true));
    let raw = Raw::new();
    let conn = raw.connect(&ir, &ia, a.addr()).await;
    let _control = open_stream(&conn, 1, &frame(true)).await;
    a.expect_hello(ir.node(), "raw").await;
    let mut old = a.transport.link(ir.node()).unwrap();
    old.send_audio(&packet(1)).unwrap();
    conn.close(0u32.into(), b"done");
    a.expect_closed(ir.node(), LinkError::Closed).await;
    let next = raw.connect(&ir, &ia, a.addr()).await;
    let _next_control = open_stream(&next, 1, &frame(false)).await;
    a.expect_hello(ir.node(), "raw").await;
    assert_eq!(old.send_audio(&packet(1)), Err(LinkError::Closed));
    assert_eq!(
        a.transport.link(ir.node()).unwrap().send_audio(&packet(1)),
        Err(LinkError::Invalid("audio is unavailable"))
    );
}
#[tokio::test]
async fn duplicate_survivor_uses_its_own_hello_on_existing_handle() {
    let one = identity();
    let two = identity();
    let (ia, ib) = if one.node() > two.node() {
        (one, two)
    } else {
        (two, one)
    };
    let mut a = node_with_hello(ia.clone(), &[&ib], advertisement("a", true));
    let mut b = node_with_hello(ib.clone(), &[&ia], advertisement("b", true));
    a.transport.connect(b.addr()).await.unwrap();
    a.expect_hello(b.id, "b").await;
    b.expect_hello(a.id, "a").await;
    let mut link = a.transport.link(b.id).unwrap();
    link.send_audio(&packet(1)).unwrap();
    assert!(matches!(b.next().await, LinkEvent::Audio { .. }));
    // The smaller client's opposite-direction connection wins rule 7 within its window.
    let raw = Raw::new();
    let survivor = raw.connect(&ib, &ia, a.addr()).await;
    let mut control = open_stream(&survivor, 1, &frame(false)).await;
    let mut ping = Vec::new();
    encode_control(&ControlMessage::Ping { t0: 99 }, &mut ping).unwrap();
    control.write_all(&ping).await.unwrap();
    // The engine already knows the link, so the survivor's Hello arrives as a refresh carrying
    // its own features (no audio), and before anything else the survivor sends.
    match a.next().await {
        LinkEvent::HelloRefresh { peer, hello } => {
            assert_eq!(peer, b.id);
            assert_eq!(hello, advertisement("raw", false));
        }
        other => panic!("expected a HelloRefresh, got {other:?}"),
    }
    assert!(matches!(
        a.next().await,
        LinkEvent::Control {
            msg: ControlMessage::Ping { t0: 99 },
            ..
        }
    ));
    assert_eq!(
        link.send_audio(&packet(1)),
        Err(LinkError::Invalid("audio is unavailable"))
    );
    survivor
        .send_datagram(encode_audio(&packet(1)).unwrap().into())
        .unwrap();
    a.expect_closed(b.id, LinkError::Invalid("audio is unavailable"))
        .await;
    assert_eq!(link.send_audio(&packet(1)), Err(LinkError::Closed));
}
#[tokio::test]
async fn audio_queue_congestion_preserves_motion_and_reliable_channels() {
    // Current-thread runtime: Quinn cannot drain its send queue during this synchronous burst.
    let (a, mut b) = pair_with_audio(true, true).await;
    let mut link = a.transport.link(b.id).unwrap();
    link.send_motion(&pointer()).unwrap();
    let mut accepted = 0;
    loop {
        match link.send_audio(&packet(400)) {
            Ok(()) => {
                accepted += 1;
                assert!(accepted < 100_000, "queue is not bounded");
            }
            Err(LinkError::Congested) => break,
            other => panic!("unexpected send result: {other:?}"),
        }
    }
    assert!(accepted > 0);
    assert_eq!(link.send_audio(&packet(400)), Err(LinkError::Congested));
    link.send_control(&ControlMessage::Ping { t0: 99 }).unwrap();
    link.send_input(&key(1)).unwrap();
    timeout(WAIT, async {
        let mut seen = [false; 3];
        while !seen.iter().all(|v| *v) {
            match b.next().await {
                LinkEvent::Audio { .. } => {}
                LinkEvent::Motion { msg, .. } => {
                    assert_eq!(msg, pointer());
                    seen[0] = true;
                }
                LinkEvent::Control { msg, .. } => {
                    assert_eq!(msg, ControlMessage::Ping { t0: 99 });
                    seen[1] = true;
                }
                LinkEvent::Input { msg, .. } => {
                    assert_eq!(msg, key(1));
                    seen[2] = true;
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
    })
    .await
    .unwrap();
}

fn audio_controls() -> [ControlMessage; 4] {
    [
        ControlMessage::AudioOpen {
            stream: AudioStreamId(2),
            kind: AudioKind::Speaker,
            channels: 2,
        },
        ControlMessage::AudioOpened {
            stream: AudioStreamId(2),
        },
        ControlMessage::AudioRefused {
            stream: AudioStreamId(2),
            reason: Refusal::InjectorFailed,
        },
        ControlMessage::AudioClose {
            stream: AudioStreamId(2),
        },
    ]
}
fn all_grants() -> Vec<Capability> {
    vec![
        Capability::WindowShare,
        Capability::AudioSpeaker,
        Capability::InputAccept,
        Capability::AudioMic,
        Capability::WindowBrowse,
    ]
}
fn grants_without_audio() -> Vec<Capability> {
    vec![
        Capability::WindowShare,
        Capability::InputAccept,
        Capability::WindowBrowse,
    ]
}
/// A real, tiny CPF1 media frame.
fn media_frame(projection: u64) -> Arc<[u8]> {
    let mut encoder = TileEncoder::new();
    let pixels = vec![0u8; 64 * 64 * 4];
    let header = FrameHeader {
        projection,
        seq: 1,
        key: true,
        captured_ns: 0,
        width: 64,
        height: 64,
    };
    let mut out = Vec::new();
    encoder
        .encode(header, &pixels, 64 * 4, false, &mut out)
        .unwrap();
    Arc::from(out)
}

async fn exchange_audio_controls(source: &Node, target: &mut Node, negotiated: bool) {
    let mut link = source.transport.link(target.id).unwrap();
    for msg in audio_controls() {
        let sent = link.send_control(&msg);
        if negotiated {
            sent.unwrap();
        } else {
            assert_eq!(sent, Err(LinkError::Invalid("audio is unavailable")));
        }
    }
    // Refused messages were never queued: the next thing the peer hears is this Ping.
    link.send_control(&ControlMessage::Ping { t0: 99 }).unwrap();
    if negotiated {
        for msg in audio_controls() {
            match target.next().await {
                LinkEvent::Control { peer, msg: got } => {
                    assert_eq!(peer, source.id);
                    assert_eq!(got, msg);
                }
                other => panic!("expected an audio control message, got {other:?}"),
            }
        }
    }
    match target.next().await {
        LinkEvent::Control {
            peer,
            msg: ControlMessage::Ping { t0: 99 },
        } => assert_eq!(peer, source.id),
        other => panic!("expected the ping, got {other:?}"),
    }
}

#[tokio::test]
async fn audio_controls_are_sent_only_when_both_sides_advertised_audio() {
    for (local, remote) in [(false, true), (true, false), (false, false), (true, true)] {
        let (mut a, mut b) = pair_with_audio(local, remote).await;
        let negotiated = local && remote;
        exchange_audio_controls(&a, &mut b, negotiated).await;
        exchange_audio_controls(&b, &mut a, negotiated).await;
    }
}

async fn refused_unnegotiated_control(msg: ControlMessage, local_audio: bool, peer_audio: bool) {
    let ia = identity();
    let ir = identity();
    let mut a = node_with_hello(ia.clone(), &[&ir], advertisement("a", local_audio));
    let raw = Raw::new();
    let conn = raw.connect(&ir, &ia, a.addr()).await;
    let mut control = open_stream(&conn, 1, &frame(peer_audio)).await;
    a.expect_hello(ir.node(), "raw").await;
    control.write_all(&control_frame(&msg)).await.unwrap();
    // The very next event is the closure: the message itself was never delivered.
    a.expect_closed(ir.node(), LinkError::Invalid("audio is unavailable"))
        .await;
    assert_eq!(closed_by_peer(&conn).await, (1, "protocol error".into()));
}

async fn delivered_negotiated_control(msg: ControlMessage) {
    let ia = identity();
    let ir = identity();
    let mut a = node_with_hello(ia.clone(), &[&ir], advertisement("a", true));
    let raw = Raw::new();
    let conn = raw.connect(&ir, &ia, a.addr()).await;
    let mut control = open_stream(&conn, 1, &frame(true)).await;
    a.expect_hello(ir.node(), "raw").await;
    control.write_all(&control_frame(&msg)).await.unwrap();
    match a.next().await {
        LinkEvent::Control { peer, msg: got } => {
            assert_eq!(peer, ir.node());
            assert_eq!(got, msg);
        }
        other => panic!("expected the audio control message, got {other:?}"),
    }
    a.expect_quiet(Duration::from_millis(150)).await;
    assert!(conn.close_reason().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unnegotiated_audio_controls_close_with_a_static_protocol_error_and_are_not_delivered() {
    let mut cases = tokio::task::JoinSet::new();
    for msg in audio_controls() {
        for (local_audio, peer_audio) in [(true, false), (false, true), (false, false)] {
            cases.spawn(refused_unnegotiated_control(
                msg.clone(),
                local_audio,
                peer_audio,
            ));
        }
        // Negotiated on both sides: delivered unchanged and the link stays open.
        cases.spawn(delivered_negotiated_control(msg));
    }
    while let Some(case) = cases.join_next().await {
        case.unwrap();
    }
}

#[tokio::test]
async fn grants_carry_audio_capabilities_only_over_a_connection_that_negotiated_audio() {
    for (local, remote) in [(true, true), (true, false), (false, true), (false, false)] {
        let (mut a, mut b) = pair_with_audio(local, remote).await;
        let expected = if local && remote {
            all_grants()
        } else {
            grants_without_audio()
        };
        a.transport
            .link(b.id)
            .unwrap()
            .send_control(&ControlMessage::Grants(all_grants()))
            .unwrap();
        match b.next().await {
            LinkEvent::Control {
                msg: ControlMessage::Grants(got),
                ..
            } => assert_eq!(got, expected, "local {local}, remote {remote}"),
            other => panic!("expected the grants, got {other:?}"),
        }
        b.transport
            .link(a.id)
            .unwrap()
            .send_control(&ControlMessage::Grants(all_grants()))
            .unwrap();
        match a.next().await {
            LinkEvent::Control {
                msg: ControlMessage::Grants(got),
                ..
            } => assert_eq!(got, expected, "local {local}, remote {remote}"),
            other => panic!("expected the grants, got {other:?}"),
        }
        // An empty or audio-only message stays a well-formed Grants message.
        a.transport
            .link(b.id)
            .unwrap()
            .send_control(&ControlMessage::Grants(vec![Capability::AudioMic]))
            .unwrap();
        match b.next().await {
            LinkEvent::Control {
                msg: ControlMessage::Grants(got),
                ..
            } => assert_eq!(
                got,
                if local && remote {
                    vec![Capability::AudioMic]
                } else {
                    Vec::new()
                }
            ),
            other => panic!("expected the grants, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn an_old_peer_only_ever_receives_what_its_decoder_knows() {
    for peer_audio in [false, true] {
        let ia = identity();
        let ir = identity();
        let mut a = node_with_hello(ia.clone(), &[&ir], advertisement("a", true));
        let raw = Raw::new();
        let conn = raw.connect(&ir, &ia, a.addr()).await;
        let _control = open_stream(&conn, 1, &frame(peer_audio)).await;
        let (mut received, hello) = RawReceiver::accept(&conn).await;
        assert_eq!(hello, advertisement("a", true));
        a.expect_hello(ir.node(), "raw").await;
        let mut link = a.transport.link(ir.node()).unwrap();
        link.send_control(&ControlMessage::Grants(all_grants()))
            .unwrap();
        for msg in audio_controls() {
            let sent = link.send_control(&msg);
            if peer_audio {
                sent.unwrap();
            } else {
                assert_eq!(sent, Err(LinkError::Invalid("audio is unavailable")));
            }
        }
        link.send_control(&ControlMessage::Ping { t0: 5 }).unwrap();
        assert_eq!(
            received.next().await,
            ControlMessage::Grants(if peer_audio {
                all_grants()
            } else {
                grants_without_audio()
            })
        );
        if peer_audio {
            for msg in audio_controls() {
                assert_eq!(received.next().await, msg);
            }
        }
        assert_eq!(received.next().await, ControlMessage::Ping { t0: 5 });
        received.expect_quiet(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn media_and_ordinary_channels_flow_whether_or_not_audio_was_negotiated() {
    for (local, remote) in [(true, true), (true, false), (false, true), (false, false)] {
        let (mut a, mut b) = pair_with_audio(local, remote).await;
        let frame = media_frame(3);
        a.transport.send_media(b.id, frame.clone()).unwrap();
        b.transport.send_media(a.id, frame.clone()).unwrap();
        for node in [&mut a, &mut b] {
            match node.next().await {
                LinkEvent::Media { data, .. } => assert_eq!(*data, *frame),
                other => panic!("expected media, got {other:?}"),
            }
        }
        for (source, target) in [(&a, &b), (&b, &a)] {
            let mut link = source.transport.link(target.id).unwrap();
            link.send_motion(&pointer()).unwrap();
            link.send_control(&ControlMessage::Ping { t0: 99 }).unwrap();
            link.send_input(&key(1)).unwrap();
        }
        let ai = a.id;
        let bi = b.id;
        tokio::join!(
            expect_mixed(&mut a, bi, None),
            expect_mixed(&mut b, ai, None)
        );
    }
}
