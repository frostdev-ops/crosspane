//! Simultaneous and repeated connections between the same two nodes (rule 7).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use crosspane_protocol::audio::{AudioPacket, AudioStreamId};
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::{Capability, ControlMessage, InputMessage, PointerMessage, Refusal};
use crosspane_security::identity::DeviceIdentity;
use crosspane_types::audio::AudioKind;
use crosspane_types::geom::PointDevice;
use crosspane_types::id::{DisplayId, SessionId};
use tokio::time::sleep;

const QUIET: Duration = Duration::from_millis(400);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_connects_leave_one_connection_and_the_engines_see_no_flap() {
    // Different start offsets exercise the different orders the two handshakes can finish in.
    for offset_us in [0u64, 150, 400, 800, 1_500, 3_000, 6_000] {
        let (mut a, mut b) = pair();
        let (from_a, from_b) = tokio::join!(a.transport.connect(b.addr()), async {
            sleep(Duration::from_micros(offset_us)).await;
            b.transport.connect(a.addr()).await
        });
        assert_eq!(from_a.unwrap(), b.id, "offset {offset_us} us");
        assert_eq!(from_b.unwrap(), a.id, "offset {offset_us} us");

        // Each engine sees exactly one Hello and never a Closed: the duplicate stays invisible.
        a.expect_hello(b.id, "b").await;
        b.expect_hello(a.id, "a").await;
        a.expect_quiet(QUIET).await;
        b.expect_quiet(QUIET).await;
        assert_eq!(a.transport.peers(), vec![b.id], "offset {offset_us} us");
        assert_eq!(b.transport.peers(), vec![a.id], "offset {offset_us} us");

        // Messages flow both ways on whichever connection survived.
        let mut a_link = a.transport.link(b.id).unwrap();
        let mut b_link = b.transport.link(a.id).unwrap();
        a_link.send_input(&key(1)).unwrap();
        a_link
            .send_control(&ControlMessage::Ping { t0: 1 })
            .unwrap();
        b_link.send_input(&key(2)).unwrap();
        b_link
            .send_control(&ControlMessage::Ping { t0: 2 })
            .unwrap();
        for (node, seq, t0) in [(&mut b, 1, 1), (&mut a, 2, 2)] {
            let mut got_input = false;
            let mut got_control = false;
            while !(got_input && got_control) {
                match node.next().await {
                    LinkEvent::Input {
                        msg: InputMessage::Key { seq: got, .. },
                        ..
                    } => {
                        assert_eq!(got, seq);
                        got_input = true;
                    }
                    LinkEvent::Control {
                        msg: ControlMessage::Ping { t0: got },
                        ..
                    } => {
                        assert_eq!(got, t0);
                        got_control = true;
                    }
                    other => panic!("unexpected event {other:?} (offset {offset_us} us)"),
                }
            }
        }

        // One close ends the whole relationship: no second connection was left behind.
        a_link.close("done");
        a.expect_closed(b.id, LinkError::Closed).await;
        b.expect_closed(a.id, LinkError::Closed).await;
        a.expect_quiet(QUIET).await;
        b.expect_quiet(QUIET).await;
        assert!(a.transport.peers().is_empty());
        assert!(b.transport.peers().is_empty());
    }
}

/// The same peer dialing again while its first connection is healthy is a redundant dial: refused
/// quietly, and the first connection carries on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_dial_by_a_healthy_peer_is_refused_quietly() {
    let ida = identity();
    let idr = identity();
    let mut a = Node::start("a", ida.clone(), &[&idr]);
    let r = idr.node();
    let raw = Raw::new();

    let first = raw.connect(&idr, &ida, a.addr()).await;
    let _control = open_stream(&first, 0x01, &hello_frame("first")).await;
    a.expect_hello(r, "first").await;
    let mut link = a.transport.link(r).unwrap();

    // The refusal can arrive before the client has even opened a stream.
    let second = raw.connect(&idr, &ida, a.addr()).await;
    let (code, reason) = closed_by_peer(&second).await;
    assert_eq!((code, reason.as_str()), (2, "duplicate"));

    // Nothing was reported, and the first connection is untouched.
    a.expect_quiet(QUIET).await;
    assert_eq!(a.transport.peers(), vec![r]);
    link.send_input(&key(1)).unwrap();
    assert!(first.close_reason().is_none());
}

/// A peer that crashed and restarted dials again while its old connection is still held. Until the
/// old connection has been silent long enough to count as gone (1.5 s) the redial is refused; once
/// it has, the new connection replaces it and the engine sees the restart: `Closed`, then a new
/// `Hello`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_peer_replaces_its_silent_old_connection() {
    let ida = identity();
    let idr = identity();
    let mut a = Node::start("a", ida.clone(), &[&idr]);
    let r = idr.node();

    let peer = CrashablePeer::connect(idr.clone(), ida.clone(), a.addr()).await;
    a.expect_hello(r, "crashed").await;
    let mut old_link = a.transport.link(r).unwrap();
    peer.crash().await;

    // Redial until the node accepts, rather than guessing how long "silent" takes: a refusal
    // closes the new connection with code 2 straight away and reports nothing.
    let raw = Raw::new();
    let started = Instant::now();
    let _control = loop {
        let attempt = raw.connect(&idr, &ida, a.addr()).await;
        let control = try_open_stream(&attempt, 0x01, &hello_frame("restarted")).await;
        tokio::select! {
            (code, _) = closed_by_peer(&attempt) => {
                assert_eq!(code, 2, "refused with the wrong code");
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "the silent connection was never replaced"
                );
                sleep(Duration::from_millis(200)).await;
            }
            event = a.events.recv() => {
                // Accepted: the engine hears the old link end, then the new one begin.
                assert!(
                    matches!(event, Some(LinkEvent::Closed { error: LinkError::Closed, .. })),
                    "{event:?}"
                );
                break control.expect("an accepted connection takes a stream");
            }
        }
    };
    assert!(
        started.elapsed() >= Duration::from_millis(500),
        "the old connection was replaced while it was still healthy"
    );
    a.expect_hello(r, "restarted").await;
    assert_eq!(old_link.send_input(&key(1)), Err(LinkError::Closed));
    a.transport.link(r).unwrap().send_input(&key(1)).unwrap();
    assert_eq!(a.transport.peers(), vec![r]);
    a.expect_quiet(QUIET).await;
}

/// An engine answers a Hello from inside the event callback. With simultaneous connects, that
/// reply must never go out on a connection that then loses the duplicate race: the Hello is
/// held until the survivor is known, so every reply arrives, and nothing flaps.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replies_sent_from_the_hello_handler_survive_simultaneous_connects() {
    let mut rounds = tokio::task::JoinSet::new();
    for round in 0..100u64 {
        rounds.spawn(async move {
            let ida = identity();
            let idb = identity();
            let mut a = Node::start_replying("a", ida.clone(), &[&idb]);
            let mut b = Node::start_replying("b", idb.clone(), &[&ida]);
            let offset_us = (round * 53) % 4_000;
            let (from_a, from_b) = tokio::join!(a.transport.connect(b.addr()), async {
                sleep(Duration::from_micros(offset_us)).await;
                b.transport.connect(a.addr()).await
            });
            assert_eq!(from_a.unwrap(), b.id, "round {round}");
            assert_eq!(from_b.unwrap(), a.id, "round {round}");
            for (node, peer, name) in [(&mut a, idb.node(), "b"), (&mut b, ida.node(), "a")] {
                node.expect_hello(peer, name).await;
                // The peer's reply to our Hello, sent from its callback.
                match node.next().await {
                    LinkEvent::Control {
                        msg: ControlMessage::Ping { t0 },
                        ..
                    } => assert_eq!(t0, REPLY_PING, "round {round}"),
                    other => panic!("round {round}: expected the peer's reply, got {other:?}"),
                }
                node.expect_quiet(Duration::from_millis(150)).await;
            }
        });
    }
    while let Some(round) = rounds.join_next().await {
        round.unwrap();
    }
}

/// Concurrent `connect`s to one address share a single attempt: one connection, one Hello each,
/// and every caller gets the peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_connects_to_one_address_share_one_attempt() {
    let (mut a, mut b) = pair();
    let target = b.addr();
    let (one, two, three) = tokio::join!(
        a.transport.connect(target),
        a.transport.connect(target),
        a.transport.connect(target)
    );
    assert_eq!(one.unwrap(), b.id);
    assert_eq!(two.unwrap(), b.id);
    assert_eq!(three.unwrap(), b.id);
    a.expect_hello(b.id, "b").await;
    b.expect_hello(a.id, "a").await;
    a.expect_quiet(QUIET).await;
    b.expect_quiet(QUIET).await;
    assert_eq!(a.transport.peers(), vec![b.id]);
    assert_eq!(b.transport.peers(), vec![a.id]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_duplicate_close_with_no_replacement_ends_the_link_after_the_grace_period() {
    let ida = identity();
    let idr = identity();
    let mut a = Node::start("a", ida.clone(), &[&idr]);
    let r = idr.node();
    let raw = Raw::new();

    let conn = raw.connect(&idr, &ida, a.addr()).await;
    let _control = open_stream(&conn, 0x01, &hello_frame("r")).await;
    a.expect_hello(r, "r").await;

    let closed_at = Instant::now();
    conn.close(2u32.into(), b"duplicate");
    // Quiet through most of the grace period, then the engine is told.
    a.expect_quiet(Duration::from_millis(1_200)).await;
    a.expect_closed(r, LinkError::Closed).await;
    assert!(
        closed_at.elapsed() >= Duration::from_millis(1_800),
        "the engine heard after only {:?}",
        closed_at.elapsed()
    );
    a.expect_quiet(QUIET).await;
}

// ---- silent supersession: HelloRefresh (WP-3.0b) -------------------------------------------------

/// Everything the local node under test advertises.
const FULL: [&str; 5] = ["e1", "audio", "h264", "h264roi", "cursor"];

fn pointer() -> PointerMessage {
    PointerMessage {
        session: SessionId(1),
        seq: 3,
        display: DisplayId(1),
        position: PointDevice::new(2.0, 4.0),
    }
}

fn packet() -> AudioPacket {
    AudioPacket {
        stream: AudioStreamId(7),
        seq: 19,
        sample_time: 48_000,
        opus: vec![0x5a; 40],
    }
}

fn all_grants() -> Vec<Capability> {
    vec![
        Capability::WindowShare,
        Capability::AudioSpeaker,
        Capability::InputAccept,
        Capability::AudioMic,
    ]
}

fn grants_without_audio() -> Vec<Capability> {
    vec![Capability::WindowShare, Capability::InputAccept]
}

/// `(larger, smaller)`: the node under test is the larger, so a connection it dialed loses rule 7
/// to one the smaller node dials.
fn ordered() -> (Arc<DeviceIdentity>, Arc<DeviceIdentity>) {
    let one = identity();
    let two = identity();
    if one.node() > two.node() {
        (one, two)
    } else {
        (two, one)
    }
}

/// `a` (the larger node) is connected to `b1`; `b2` is the same identity dialing from another
/// endpoint, so its connection has the smaller client id and silently supersedes the first. The
/// engine of `a` must see one ordinary `Hello` (from `b1`), then `HelloRefresh` with `b2`'s own
/// features before anything else `b2` sends, and no `Closed` at all.
async fn refresh_case(first: &[&str], second: &[&str]) {
    let (ia, ib) = ordered();
    let mut a = node_with_hello(ia.clone(), &[&ib], hello_with("a", &FULL));
    let mut b1 = node_with_hello(ib.clone(), &[&ia], hello_with("b1", first));
    let mut b2 = node_with_hello(ib.clone(), &[&ia], hello_with("b2", second));
    let b = ib.node();
    let negotiated = second.contains(&"audio");
    let what = format!("{first:?} -> {second:?}");

    assert_eq!(a.transport.connect(b1.addr()).await.unwrap(), b);
    a.expect_hello(b, "b1").await;
    b1.expect_hello(a.id, "a").await;
    let mut old = a.transport.link(b).unwrap();
    // Nothing but the one ordinary Hello so far.
    a.expect_quiet(Duration::from_millis(100)).await;

    assert_eq!(b2.transport.connect(a.addr()).await.unwrap(), a.id);
    b2.expect_hello(a.id, "a").await;
    let mut from_b2 = b2.transport.link(a.id).unwrap();
    // Sent at once, right behind b2's Hello, on all three channels.
    from_b2
        .send_control(&ControlMessage::Ping { t0: 7 })
        .unwrap();
    from_b2.send_input(&key(1)).unwrap();
    from_b2.send_motion(&pointer()).unwrap();
    if negotiated {
        from_b2.send_audio(&packet()).unwrap();
    }

    // The refresh is the survivor's own Hello, before anything else it sent.
    match a.next().await {
        LinkEvent::HelloRefresh { peer, hello } => {
            assert_eq!(peer, b, "{what}");
            assert_eq!(hello, hello_with("b2", second), "{what}");
        }
        other => panic!("{what}: expected a HelloRefresh first, got {other:?}"),
    }
    // (That a replaced connection can deliver nothing is checked directly, with traffic from an
    // obsolete connection id, by `hub::tests::an_obsolete_connection_delivers_nothing_and_changes_nothing`.)
    let mut seen = [false; 4];
    seen[3] = !negotiated;
    while !seen.iter().all(|v| *v) {
        match a.next().await {
            LinkEvent::Control {
                msg: ControlMessage::Ping { t0: 7 },
                ..
            } => seen[0] = true,
            LinkEvent::Input { msg, .. } => {
                assert_eq!(msg, key(1), "{what}");
                seen[1] = true;
            }
            LinkEvent::Motion { msg, .. } => {
                assert_eq!(msg, pointer(), "{what}");
                seen[2] = true;
            }
            LinkEvent::Audio { packet: got, .. } => {
                assert_eq!(got, packet(), "{what}");
                seen[3] = true;
            }
            other => panic!("{what}: unexpected event {other:?}"),
        }
    }
    a.expect_quiet(QUIET).await;

    // The handle the engine already holds follows the link onto the survivor, and is gated by the
    // survivor's Hello, not by the connection it replaced.
    old.send_control(&ControlMessage::Ping { t0: 5 }).unwrap();
    let audio_controls = [
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
    ];
    for msg in &audio_controls {
        let sent = old.send_control(msg);
        if negotiated {
            sent.unwrap();
        } else {
            assert_eq!(
                sent,
                Err(LinkError::Invalid("audio is unavailable")),
                "{what}"
            );
        }
    }
    let sent = old.send_audio(&packet());
    if negotiated {
        sent.unwrap();
    } else {
        assert_eq!(
            sent,
            Err(LinkError::Invalid("audio is unavailable")),
            "{what}"
        );
    }
    old.send_control(&ControlMessage::Grants(all_grants()))
        .unwrap();
    let (mut ping, mut grants, mut audio) = (false, false, !negotiated);
    let mut controls = Vec::new();
    while !(ping && grants && audio) {
        match b2.next().await {
            LinkEvent::Control {
                msg: ControlMessage::Ping { t0: 5 },
                ..
            } => ping = true,
            LinkEvent::Control {
                msg: ControlMessage::Grants(got),
                ..
            } => {
                // Audio capabilities reach only a peer that negotiated audio.
                assert_eq!(
                    got,
                    if negotiated {
                        all_grants()
                    } else {
                        grants_without_audio()
                    },
                    "{what}"
                );
                grants = true;
            }
            LinkEvent::Control { msg, .. } => controls.push(msg),
            LinkEvent::Audio { packet: got, .. } => {
                assert_eq!(got, packet(), "{what}");
                audio = true;
            }
            other => panic!("{what}: b2 got {other:?}"),
        }
    }
    // The audio control messages arrived, in order, exactly when audio was negotiated.
    assert_eq!(
        controls,
        if negotiated {
            audio_controls.to_vec()
        } else {
            Vec::new()
        },
        "{what}"
    );
    b2.expect_quiet(Duration::from_millis(150)).await;
    a.expect_quiet(Duration::from_millis(150)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_superseding_connection_with_the_same_features_still_refreshes() {
    refresh_case(&["e1", "audio", "h264"], &["e1", "audio", "h264"]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refresh_that_drops_audio_gates_the_existing_handle_and_filters_grants() {
    refresh_case(&["e1", "audio", "h264"], &["e1", "h264"]).await;
    refresh_case(&["e1", "audio", "h264roi", "cursor"], &["e1"]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refresh_that_gains_audio_enables_it_on_the_existing_handle() {
    refresh_case(&["e1"], &["e1", "audio", "h264", "h264roi", "cursor"]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refresh_that_changes_codec_and_cursor_features_reports_them() {
    refresh_case(
        &["e1", "audio", "h264"],
        &["e1", "audio", "h264roi", "cursor"],
    )
    .await;
}

/// A connection that is registered but whose Hello has not arrived is replaced by a connection the
/// node dialed itself: the engine never heard of the link, so it gets one ordinary `Hello` (the
/// survivor's), never a refresh, never a `Closed`.
///
/// Nothing here depends on timing. A controlled raw peer is the original: it connects, opens its
/// control stream and deliberately sends no Hello, and the node's own streams reaching it prove the
/// node registered the connection. The node then dials a real peer with the smaller client id, and
/// the original being closed as a duplicate proves the supersession happened before anything else
/// is checked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_connection_replaced_before_its_hello_yields_one_ordinary_hello_and_no_refresh() {
    for round in 0..3 {
        // `ia` is the smaller node: the node under test dials with the smaller client id, so its
        // connection beats the raw peer's (the larger client).
        let (ib, ia) = ordered();
        let mut a = node_with_hello(ia.clone(), &[&ib], hello_with("a", &FULL));
        let mut b = node_with_hello(
            ib.clone(),
            &[&ia],
            hello_with("survivor", &["e1", "audio", "h264"]),
        );
        let peer = ib.node();

        // The original: registered at the node, Hello withheld.
        let raw = Raw::new();
        let original = raw.connect(&ib, &ia, a.addr()).await;
        let _control = open_stream(&original, 0x01, &[]).await;
        let (_from_node, node_hello) = RawReceiver::accept(&original).await;
        assert_eq!(node_hello, hello_with("a", &FULL), "round {round}");
        a.expect_quiet(Duration::from_millis(150)).await;

        // The survivor: the node's own dial.
        assert_eq!(a.transport.connect(b.addr()).await.unwrap(), peer);
        // The supersession happened: the original, still without a Hello, was closed as a
        // duplicate (a refused survivor would have left it open).
        let (code, reason) = closed_by_peer(&original).await;
        assert_eq!((code, reason.as_str()), (2, "duplicate"), "round {round}");

        // The engine's whole history: the survivor's one ordinary Hello, then nothing.
        match a.next().await {
            LinkEvent::Control {
                peer: from,
                msg: ControlMessage::Hello(hello),
            } => {
                assert_eq!(from, peer, "round {round}");
                assert_eq!(
                    hello,
                    hello_with("survivor", &["e1", "audio", "h264"]),
                    "round {round}"
                );
            }
            other => panic!("round {round}: expected the survivor's Hello, got {other:?}"),
        }
        b.expect_hello(a.id, "a").await;
        a.expect_quiet(QUIET).await;
        assert_eq!(a.transport.peers(), vec![peer], "round {round}");
        // The link works over the survivor, whose features were negotiated on its own Hello.
        let mut link = a.transport.link(peer).unwrap();
        link.send_audio(&packet()).unwrap();
        link.send_control(&ControlMessage::Ping { t0: 3 }).unwrap();
        // A datagram and a stream message arrive in either order.
        let (mut audio, mut ping) = (false, false);
        while !(audio && ping) {
            match b.next().await {
                LinkEvent::Audio { packet: got, .. } => {
                    assert_eq!(got, packet(), "round {round}");
                    audio = true;
                }
                LinkEvent::Control {
                    msg: ControlMessage::Ping { t0: 3 },
                    ..
                } => ping = true,
                other => panic!("round {round}: unexpected event {other:?}"),
            }
        }
    }
}

/// A connection that loses rule 7 is refused: its Hello and everything after it never reach the
/// engine, and the established link is undisturbed (no refresh, no Closed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rejected_connection_delivers_no_refresh_and_no_events() {
    let (ib, ia) = ordered();
    // `ia` is the smaller node here: the node under test dialed first, and its client id wins.
    let mut a = node_with_hello(ia.clone(), &[&ib], hello_with("a", &FULL));
    let mut b = node_with_hello(ib.clone(), &[&ia], hello_with("b", &FULL));
    assert_eq!(a.transport.connect(b.addr()).await.unwrap(), b.id);
    a.expect_hello(b.id, "b").await;
    b.expect_hello(a.id, "a").await;
    let mut link = a.transport.link(b.id).unwrap();

    // The larger node dials again from another endpoint (a different features list, too).
    let raw = Raw::new();
    let loser = raw.connect(&ib, &ia, a.addr()).await;
    let _control = try_open_stream(&loser, 0x01, &hello_frame_with("loser", &["e1"])).await;
    let (code, reason) = closed_by_peer(&loser).await;
    assert_eq!((code, reason.as_str()), (2, "duplicate"));
    a.expect_quiet(QUIET).await;
    b.expect_quiet(Duration::from_millis(100)).await;
    // The first connection carries on, in both directions.
    link.send_control(&ControlMessage::Ping { t0: 1 }).unwrap();
    assert!(matches!(
        b.next().await,
        LinkEvent::Control {
            msg: ControlMessage::Ping { t0: 1 },
            ..
        }
    ));
    b.transport
        .link(a.id)
        .unwrap()
        .send_control(&ControlMessage::Ping { t0: 2 })
        .unwrap();
    assert!(matches!(
        a.next().await,
        LinkEvent::Control {
            msg: ControlMessage::Ping { t0: 2 },
            ..
        }
    ));
    a.expect_quiet(QUIET).await;
}

/// Only authenticated peers can supersede: a connection with an unpinned key never gets as far as a
/// Hello, so the engine is not refreshed and the link is untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unauthenticated_connection_cannot_refresh_an_established_link() {
    let (ia, ib) = ordered();
    let stranger = identity();
    let mut a = node_with_hello(ia.clone(), &[&ib], hello_with("a", &FULL));
    let b = node_with_hello(ib.clone(), &[&ia], hello_with("b", &FULL));
    a.transport.connect(b.addr()).await.unwrap();
    a.expect_hello(b.id, "b").await;
    let raw = Raw::new();
    if let Ok(conn) = raw
        .connect_with_alpn(&stranger, &ia, a.addr(), crosspane_protocol::ALPN)
        .await
    {
        let _ = try_open_stream(&conn, 0x01, &hello_frame_with("stranger", &FULL)).await;
    }
    a.expect_quiet(QUIET).await;
    assert_eq!(a.transport.peers(), vec![b.id]);
}
