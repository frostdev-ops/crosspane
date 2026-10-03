#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use common::*;
use crosspane_protocol::link::LinkEvent;
use crosspane_protocol::msg::{ControlMessage, InputMessage};
use crosspane_transport::TransportError;
use crosspane_types::id::NodeId;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_sides_get_hello_first_with_the_right_node_id() {
    let (mut a, mut b) = pair();
    let peer = a.transport.connect(b.addr()).await.unwrap();
    assert_eq!(peer, b.id);

    a.expect_hello(b.id, "b").await;
    b.expect_hello(a.id, "a").await;
    assert_eq!(a.transport.peers(), vec![b.id]);
    assert_eq!(b.transport.peers(), vec![a.id]);
    assert_eq!(a.transport.link(b.id).unwrap().peer(), b.id);
    assert_eq!(b.transport.link(a.id).unwrap().peer(), a.id);
    assert!(a.transport.link(NodeId([9; 32])).is_none());

    // Nothing but the two hellos.
    a.expect_quiet(Duration::from_millis(300)).await;
    b.expect_quiet(Duration::from_millis(300)).await;
}

/// Input sent the moment the connection is up still arrives after the Hello (the engine is
/// promised a Hello first), however the receiver schedules its streams.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hello_comes_first_even_when_input_is_sent_immediately() {
    let mut rounds = tokio::task::JoinSet::new();
    for round in 0..25 {
        rounds.spawn(async move {
            let (mut a, mut b) = pair();
            a.transport.connect(b.addr()).await.unwrap();
            let mut link = a.transport.link(b.id).unwrap();
            for seq in 1..=20 {
                link.send_input(&key(seq)).unwrap();
            }
            link.send_control(&ControlMessage::Ping { t0: 1 }).unwrap();
            b.expect_hello(a.id, "a").await;
            let (mut next_key, mut pings) = (1, 0);
            while next_key <= 20 || pings < 1 {
                match b.next().await {
                    LinkEvent::Input {
                        msg: InputMessage::Key { seq, .. },
                        ..
                    } => {
                        assert_eq!(seq, next_key, "round {round}");
                        next_key += 1;
                    }
                    LinkEvent::Control {
                        msg: ControlMessage::Ping { .. },
                        ..
                    } => pings += 1,
                    other => panic!("round {round}: unexpected {other:?}"),
                }
            }
            a.expect_hello(b.id, "b").await;
        });
    }
    while let Some(round) = rounds.join_next().await {
        round.unwrap();
    }
}

/// A second address of a peer we are already connected to is a quiet no-op: the existing peer is
/// returned, nothing is reported, and the first connection carries on. The two addresses are
/// 127.0.0.1 and [::1] of one dual-stack node.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_address_of_a_connected_peer_is_a_quiet_no_op() {
    if !dual_stack_available("a_second_address_of_a_connected_peer_is_a_quiet_no_op") {
        return;
    }
    let ida = identity();
    let idb = identity();
    let (pins_a, audit_a) = audited_pins(&[&idb]);
    let (pins_b, audit_b) = audited_pins(&[&ida]);
    let mut a = Node::start_with("a", ida.clone(), pins_a, dual_stack(), false);
    let mut b = Node::start_with("b", idb.clone(), pins_b, dual_stack(), false);
    let (first, second) = both_loopbacks(b.addr().port());

    assert_eq!(
        audited_connect(&a, first, [&audit_a, &audit_b], "second-address first dial")
            .await
            .unwrap(),
        b.id
    );
    a.expect_hello(b.id, "b").await;
    b.expect_hello(a.id, "a").await;
    let mut link = a.transport.link(b.id).unwrap();

    assert_eq!(
        audited_connect(
            &a,
            second,
            [&audit_a, &audit_b],
            "second-address second dial"
        )
        .await
        .unwrap(),
        b.id
    );
    a.expect_quiet(Duration::from_millis(500)).await;
    b.expect_quiet(Duration::from_millis(500)).await;
    assert_eq!(a.transport.peers(), vec![b.id]);
    assert_eq!(b.transport.peers(), vec![a.id]);
    link.send_control(&ControlMessage::Ping { t0: 3 }).unwrap();
    assert!(matches!(
        b.next().await,
        LinkEvent::Control {
            msg: ControlMessage::Ping { t0: 3 },
            ..
        }
    ));
}

/// Characterize dual-stack fixture admission without relying on random ephemeral-port reuse.
/// Every address below belongs to a node this test starts, including the IPv4 occupant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dual_stack_fixture_first_dial_cannot_reach_an_owned_ipv4_port_occupant() {
    use std::sync::Arc;

    use crosspane_transport::{Transport, TransportConfig};
    use tokio::sync::mpsc::unbounded_channel;

    if !dual_stack_available(
        "a_dual_stack_fixture_first_dial_cannot_reach_an_owned_ipv4_port_occupant",
    ) {
        return;
    }
    let (ida, idb, idc) = (identity(), identity(), identity());
    let occupant = Node::start("ipv4 occupant", idc, &[&ida]);
    let occupied_port = occupant.addr().port();
    let (send, _events) = unbounded_channel();
    let (pins_b, audit_b) = audited_pins(&[&ida]);
    let bound = Transport::bind(
        TransportConfig {
            bind: std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, occupied_port)),
            identity: idb.clone(),
            pins: pins_b,
            hello: hello("ipv6 wildcard"),
        },
        Arc::new(move |event| {
            let _ = send.send(event);
        }),
    );
    let wildcard = match bound {
        Ok(transport) => transport,
        Err(TransportError::Bind(error)) if error.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!("dual-stack fixture refuses an already-owned IPv4 port");
            return;
        }
        Err(error) => panic!("unexpected wildcard fixture bind failure: {error:?}"),
    };
    let (pins, audit) = audited_pins(&[&idb]);
    let a = Node::start_with("a", ida, pins, loopback(), false);
    let result = audited_connect(
        &a,
        both_loopbacks(wildcard.local_addr().port()).0,
        [&audit, &audit_b],
        "owned IPv4/IPv6 same-port first dial",
    )
    .await;
    assert!(
        matches!(result, Ok(peer) if peer == idb.node()),
        "IPv6 wildcard bound over our IPv4 occupant; first dial result={result:?}"
    );
}

/// The NodeId a pin store returns must be the hash of the key it was asked about.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pin_store_that_misnames_a_key_is_refused() {
    use std::sync::Arc;

    struct Liar(Vec<u8>);
    impl crosspane_transport::PinStore for Liar {
        fn trusted(&self, spki: &[u8]) -> Option<NodeId> {
            (spki == self.0).then_some(NodeId([7; 32]))
        }
    }

    let ida = identity();
    let idb = identity();
    let a = Node::start_with(
        "a",
        ida.clone(),
        Arc::new(Liar(idb.spki().to_vec())),
        loopback(),
        false,
    );
    let mut b = Node::start("b", idb.clone(), &[&ida]);
    assert!(matches!(
        a.transport.connect(b.addr()).await,
        Err(TransportError::Untrusted)
    ));
    assert!(a.transport.peers().is_empty());
    // Whatever B saw, it didn't treat A as a peer it greeted.
    while b.events.try_recv().is_ok() {}
}

/// An engine reacts to a Hello by fetching the link and replying. The sink runs while the
/// transport is mid-delivery, so calling back into it must not deadlock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sink_may_call_back_into_the_transport() {
    use std::sync::{Arc, OnceLock};

    use crosspane_protocol::link::LinkEventSink;
    use crosspane_transport::{Transport, TransportConfig};
    use tokio::sync::mpsc::unbounded_channel;

    let ida = identity();
    let idb = identity();
    let (tx, mut events_b) = unbounded_channel();
    let slot: Arc<OnceLock<Arc<Transport>>> = Arc::new(OnceLock::new());
    let sink: LinkEventSink = {
        let slot = slot.clone();
        Arc::new(move |event: LinkEvent| {
            if let (Some(transport), LinkEvent::Control { peer, .. }) = (slot.get(), &event) {
                assert_eq!(transport.peers(), vec![*peer]);
                let mut link = transport
                    .link(*peer)
                    .expect("a link inside the hello handler");
                link.send_control(&ControlMessage::Ping { t0: 77 }).unwrap();
            }
            let _ = tx.send(event);
        })
    };
    let b = Arc::new(
        Transport::bind(
            TransportConfig {
                bind: loopback(),
                identity: idb.clone(),
                pins: Pins::of(&[&ida]),
                hello: hello("b"),
            },
            sink,
        )
        .unwrap(),
    );
    slot.set(b.clone()).unwrap();
    let mut a = Node::start("a", ida.clone(), &[&idb]);

    a.transport.connect(b.local_addr()).await.unwrap();
    a.expect_hello(idb.node(), "b").await;
    // B's Hello handler replied.
    assert!(matches!(
        a.next().await,
        LinkEvent::Control {
            msg: ControlMessage::Ping { t0: 77 },
            ..
        }
    ));
    assert!(matches!(
        next_event(&mut events_b).await,
        LinkEvent::Control {
            msg: ControlMessage::Hello(_),
            ..
        }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connecting_again_returns_the_existing_peer() {
    let (mut a, mut b) = connected_pair().await;
    // Either side asking for the other's address finds the live connection.
    assert_eq!(a.transport.connect(b.addr()).await.unwrap(), b.id);
    assert_eq!(b.transport.connect(a.addr()).await.unwrap(), a.id);
    a.expect_quiet(Duration::from_millis(300)).await;
    b.expect_quiet(Duration::from_millis(300)).await;
    assert_eq!(a.transport.peers(), vec![b.id]);
    assert_eq!(b.transport.peers(), vec![a.id]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unpinned_key_is_refused_in_both_directions() {
    let ida = identity();
    let idb = identity();
    let idc = identity();
    let mut a = Node::start("a", ida.clone(), &[&idb]);
    let mut b = Node::start("b", idb.clone(), &[&ida]);
    // C pins A, but A does not pin C.
    let mut c = Node::start("c", idc.clone(), &[&ida]);

    // To an unpinned server: A refuses C's key.
    assert!(matches!(
        a.transport.connect(c.addr()).await,
        Err(TransportError::Untrusted)
    ));
    // From an unpinned client: C is refused by A, and A reports nothing.
    assert!(matches!(
        c.transport.connect(a.addr()).await,
        Err(TransportError::Untrusted)
    ));

    for node in [&mut a, &mut b, &mut c] {
        node.expect_quiet(Duration::from_millis(300)).await;
        assert!(node.transport.peers().is_empty());
    }

    // The pinned pair is unaffected.
    assert_eq!(a.transport.connect(b.addr()).await.unwrap(), b.id);
    a.expect_hello(b.id, "b").await;
    b.expect_hello(a.id, "a").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_with_no_pinned_servers_is_untrusted_by_name() {
    // Neither side pins the other.
    let a = Node::start("a", identity(), &[]);
    let mut b = Node::start("b", identity(), &[]);
    assert!(matches!(
        a.transport.connect(b.addr()).await,
        Err(TransportError::Untrusted)
    ));
    b.expect_quiet(Duration::from_millis(300)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_wrong_alpn_is_refused_and_reported_nowhere() {
    let ida = identity();
    let idr = identity();
    let mut a = Node::start("a", ida.clone(), &[&idr]);
    let raw = Raw::new();
    let result = raw
        .connect_with_alpn(&idr, &ida, a.addr(), b"crosspane/2")
        .await;
    assert!(result.is_err(), "ALPN crosspane/2 must not connect");
    a.expect_quiet(Duration::from_millis(300)).await;
    assert!(a.transport.peers().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_presents_no_key_is_refused() {
    let ida = identity();
    let mut a = Node::start("a", ida.clone(), &[&identity()]);
    let raw = Raw::new();
    // Refused by TLS itself (a handshake alert), not merely closed afterwards by the application.
    match raw.connect_without_key(&ida, a.addr()).await {
        Err(
            quinn::ConnectionError::ConnectionClosed(_) | quinn::ConnectionError::TransportError(_),
        ) => {}
        other => panic!("mutual authentication must be mandatory in TLS, got {other:?}"),
    }
    a.expect_quiet(Duration::from_millis(300)).await;
    assert!(a.transport.peers().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connecting_to_nothing_times_out() {
    let (a, b) = pair();
    let silent = b.addr();
    drop(b);
    let started = Instant::now();
    let error = a.transport.connect(silent).await.unwrap_err();
    assert!(
        matches!(error, TransportError::Timeout | TransportError::Connect(_)),
        "{error:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(7));
}

/// How long `connect` takes on loopback. A connection whose client has the larger NodeId is held
/// for the 300 ms settle time before it counts as established, so the two groups differ.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loopback_connect_time() {
    let (mut immediate, mut held) = (Vec::new(), Vec::new());
    for _ in 0..40 {
        let (a, b) = pair();
        let started = Instant::now();
        a.transport.connect(b.addr()).await.unwrap();
        let took = started.elapsed();
        if a.id < b.id {
            immediate.push(took);
        } else {
            held.push(took);
        }
    }
    for (name, samples) in [
        ("smaller-id dialer", &mut immediate),
        ("larger-id dialer", &mut held),
    ] {
        samples.sort();
        if let (Some(first), Some(last)) = (samples.first(), samples.last()) {
            eprintln!(
                "loopback connect, {name}: {} samples, min {first:?}, median {:?}, max {last:?}",
                samples.len(),
                samples[samples.len() / 2]
            );
        }
    }
    // Generous bounds for a slow CI machine; they still catch a connect that stalls for seconds.
    assert!(immediate.iter().all(|took| *took < Duration::from_secs(1)));
    assert!(held.iter().all(|took| *took < Duration::from_secs(2)));
}
