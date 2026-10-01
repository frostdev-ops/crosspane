//! Synthetic encoded packets only; all sockets belong to these loopback tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::*;
use crosspane_protocol::audio::{AudioPacket, AudioStreamId, encode_audio};
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::{ControlMessage, Hello, PointerMessage};
use crosspane_protocol::wire::encode_control;
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
            LinkEvent::Audio {
                peer: from,
                packet: got,
            } => {
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
