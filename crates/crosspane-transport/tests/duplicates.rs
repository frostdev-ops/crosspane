//! Simultaneous and repeated connections between the same two nodes (rule 7).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use common::*;
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::{ControlMessage, InputMessage};
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

/// A peer that crashed and restarted dials again while its old connection is still held. The
/// old connection has gone silent, so the new one replaces it, and the engine sees the restart:
/// `Closed`, then a new `Hello`.
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

    // Silent for long enough to count as gone (1.5 s, sampled every 250 ms).
    sleep(Duration::from_millis(2_000)).await;

    let raw = Raw::new();
    let second = raw.connect(&idr, &ida, a.addr()).await;
    let _control = open_stream(&second, 0x01, &hello_frame("restarted")).await;
    a.expect_closed(r, LinkError::Closed).await;
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
