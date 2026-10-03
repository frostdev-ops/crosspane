//! Checks that need the transport's internals.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use tokio::time::sleep;

use crate::Transport;
use crate::common::{Node, pair};

/// Connections still open on the endpoint, closing ones included until they drain.
fn open_connections(transport: &Transport) -> usize {
    transport.inner.endpoint.open_connections()
}

/// Rule 7: after simultaneous connects exactly one QUIC connection survives on each side. The
/// loser must really be closed, not just forgotten.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_connects_leave_exactly_one_quic_connection_per_side() {
    for offset_us in [0u64, 200, 700, 1_500, 4_000] {
        let (a, b) = pair();
        let (from_a, from_b) = tokio::join!(a.transport.connect(b.addr()), async {
            sleep(Duration::from_micros(offset_us)).await;
            b.transport.connect(a.addr()).await
        });
        from_a.unwrap();
        from_b.unwrap();

        // The loser drains for a moment after it closes.
        let deadline = Instant::now() + Duration::from_secs(5);
        while open_connections(&a.transport) != 1 || open_connections(&b.transport) != 1 {
            assert!(
                Instant::now() < deadline,
                "offset {offset_us} us: connections left: a={}, b={}",
                open_connections(&a.transport),
                open_connections(&b.transport)
            );
            sleep(Duration::from_millis(20)).await;
        }
        sleep(Duration::from_millis(500)).await;
        assert_eq!(open_connections(&a.transport), 1, "offset {offset_us} us");
        assert_eq!(open_connections(&b.transport), 1, "offset {offset_us} us");
        assert_eq!(a.transport.peers(), vec![b.id]);
        assert_eq!(b.transport.peers(), vec![a.id]);
    }
}

/// A refused or failed handshake leaves nothing registered or open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refused_handshakes_leave_no_connections() {
    use crate::TransportError;
    let ida = crate::common::identity();
    let a = Node::start("a", ida.clone(), &[]);
    let b = Node::start("b", crate::common::identity(), &[&ida]);
    assert!(matches!(
        b.transport.connect(a.addr()).await,
        Err(TransportError::Untrusted)
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    while open_connections(&a.transport) != 0 || open_connections(&b.transport) != 0 {
        assert!(
            Instant::now() < deadline,
            "connections left after a refusal"
        );
        sleep(Duration::from_millis(20)).await;
    }
    assert!(a.transport.peers().is_empty() && b.transport.peers().is_empty());
}

/// 0-RTT is never available, even on repeat connections to a peer we already know: input messages
/// must not be replayable (04 §3).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_rtt_is_never_available() {
    let (a, b) = pair();
    for round in 0..3 {
        let connecting = a
            .transport
            .inner
            .endpoint
            .connect_with(
                a.transport.inner.client_config.clone(),
                b.addr(),
                "crosspane",
            )
            .unwrap();
        let Err(connecting) = connecting.into_0rtt() else {
            panic!("round {round}: 0-RTT unexpectedly available");
        };
        let conn = connecting.await.unwrap();
        // Stay up long enough to receive any session ticket the server would send.
        sleep(Duration::from_millis(200)).await;
        conn.close(0u32.into(), b"done");
        sleep(Duration::from_millis(50)).await;
    }
}

/// Drain `node`'s events for `quiet` after the last one: at most one `HelloRefresh` from `peer` (a
/// connection that silently took the link over, WP-3.0b) may appear; a `Closed` or a second
/// ordinary `Hello` fails the test.
async fn expect_at_most_one_refresh(
    node: &mut Node,
    peer: crosspane_types::id::NodeId,
    quiet: Duration,
    round: u32,
) {
    use crosspane_protocol::link::LinkEvent;

    let mut refreshes = 0;
    while let Ok(event) = tokio::time::timeout(quiet, node.events.recv()).await {
        match event {
            Some(LinkEvent::HelloRefresh { peer: from, .. }) => {
                assert_eq!(from, peer, "round {round}");
                refreshes += 1;
                assert!(refreshes <= 1, "round {round}: more than one refresh");
            }
            other => panic!("round {round}: unexpected event {other:?}"),
        }
    }
}

/// A competing connection that shows up well after the first one settled (so the engines have
/// already been told about the link) is resolved by rule 7 without the engines noticing a close:
/// no `Closed`, no second ordinary `Hello` (a connection that takes the link over refreshes the
/// engine once with `HelloRefresh`), existing handles keep working, and exactly one connection is
/// left.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_competing_connection_takes_over_or_gives_way_silently() {
    use crosspane_protocol::link::LinkEvent;
    use crosspane_protocol::msg::ControlMessage;

    use crate::common::{
        audited_connect, audited_pins, both_loopbacks, dual_stack, dual_stack_available, identity,
    };

    if !dual_stack_available("a_late_competing_connection_takes_over_or_gives_way_silently") {
        return;
    }

    let mut rounds = tokio::task::JoinSet::new();
    // Fresh random identities put the smaller NodeId on either side; several rounds cover both.
    for round in 0..8 {
        rounds.spawn(async move {
            let (ida, idb) = (identity(), identity());
            // Dual-stack nodes: 127.0.0.1 and [::1] are two addresses of one node.
            let (pins_a, audit_a) = audited_pins(&[&idb]);
            let (pins_b, audit_b) = audited_pins(&[&ida]);
            let mut a = Node::start_with("a", ida.clone(), pins_a, dual_stack(), false);
            let mut b = Node::start_with("b", idb.clone(), pins_b, dual_stack(), false);

            // A dials B; both engines are told, and fetch their handles.
            audited_connect(
                &a,
                both_loopbacks(b.addr().port()).0,
                [&audit_a, &audit_b],
                &format!("late-competing round {round} first dial"),
            )
            .await
            .unwrap();
            a.expect_hello(b.id, "b").await;
            b.expect_hello(a.id, "a").await;
            let mut a_link = a.transport.link(b.id).unwrap();
            let mut b_link = b.transport.link(a.id).unwrap();

            // Then B dials A at another address of A: a competing connection in the opposite
            // direction.
            assert_eq!(
                audited_connect(
                    &b,
                    both_loopbacks(a.addr().port()).1,
                    [&audit_b, &audit_a],
                    &format!("late-competing round {round} competing dial"),
                )
                .await
                .unwrap(),
                a.id,
                "round {round}"
            );

            let quiet = Duration::from_millis(500);
            expect_at_most_one_refresh(&mut a, b.id, quiet, round).await;
            expect_at_most_one_refresh(&mut b, a.id, quiet, round).await;
            assert_eq!(a.transport.peers(), vec![b.id], "round {round}");
            assert_eq!(b.transport.peers(), vec![a.id], "round {round}");
            a_link
                .send_control(&ControlMessage::Ping { t0: 11 })
                .unwrap();
            b_link
                .send_control(&ControlMessage::Ping { t0: 22 })
                .unwrap();
            assert!(matches!(
                b.next().await,
                LinkEvent::Control {
                    msg: ControlMessage::Ping { t0: 11 },
                    ..
                }
            ));
            assert!(matches!(
                a.next().await,
                LinkEvent::Control {
                    msg: ControlMessage::Ping { t0: 22 },
                    ..
                }
            ));

            // The loser really closed.
            let deadline = Instant::now() + Duration::from_secs(5);
            while open_connections(&a.transport) != 1 || open_connections(&b.transport) != 1 {
                assert!(
                    Instant::now() < deadline,
                    "round {round}: connections left: a={}, b={}",
                    open_connections(&a.transport),
                    open_connections(&b.transport)
                );
                sleep(Duration::from_millis(20)).await;
            }
        });
    }
    while let Some(round) = rounds.join_next().await {
        round.unwrap();
    }
}

/// The first contact with a node is answered with a retry (address validation); the client
/// remembers the token, so later connections skip it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unvalidated_senders_are_sent_a_retry_before_any_handshake_state_exists() {
    let (a, b) = pair();
    a.transport.connect(b.addr()).await.unwrap();
    assert!(
        b.transport
            .inner
            .retries
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 1,
        "no retry was sent to a sender with an unvalidated address"
    );
}

/// Concurrent `connect`s to one address make one handshake, not one each.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_connects_make_a_single_handshake() {
    let (a, b) = pair();
    let target = b.addr();
    let (one, two, three) = tokio::join!(
        a.transport.connect(target),
        a.transport.connect(target),
        a.transport.connect(target)
    );
    assert_eq!(one.unwrap(), b.id);
    assert_eq!(two.unwrap(), b.id);
    assert_eq!(three.unwrap(), b.id);
    assert_eq!(a.transport.inner.endpoint.stats().outgoing_handshakes, 1);
}

/// On a dual-stack endpoint an IPv4 peer is reported IPv4-mapped; the same peer written either way
/// is one address, so asking again finds the connection instead of dialing a second time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_peer_written_two_ways_is_one_address() {
    use crate::common::{Pins, dual_stack, dual_stack_available, identity};

    if !dual_stack_available("one_peer_written_two_ways_is_one_address") {
        return;
    }
    let (ida, idb) = (identity(), identity());
    let a = Node::start_with("a", ida.clone(), Pins::of(&[&idb]), dual_stack(), false);
    let b = Node::start_with("b", idb.clone(), Pins::of(&[&ida]), dual_stack(), false);
    let port = b.addr().port();
    let plain: std::net::SocketAddr = ([127, 0, 0, 1], port).into();
    let mapped: std::net::SocketAddr = format!("[::ffff:127.0.0.1]:{port}").parse().unwrap();

    assert_eq!(a.transport.connect(plain).await.unwrap(), b.id);
    assert_eq!(a.transport.connect(mapped).await.unwrap(), b.id);
    assert_eq!(a.transport.inner.endpoint.stats().outgoing_handshakes, 1);
}
