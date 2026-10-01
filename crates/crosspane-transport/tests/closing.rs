#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use crosspane_protocol::link::{LinkError, LinkEvent, PeerLink};
use crosspane_protocol::msg::{ControlMessage, InputMessage};
use crosspane_transport::TransportError;

const QUIET: Duration = Duration::from_millis(400);

fn assert_all_sends_closed(link: &mut dyn PeerLink) {
    assert_eq!(link.send_input(&key(1)), Err(LinkError::Closed));
    assert_eq!(
        link.send_control(&ControlMessage::Ping { t0: 1 }),
        Err(LinkError::Closed)
    );
    let pointer = crosspane_protocol::msg::PointerMessage {
        session: crosspane_types::id::SessionId(1),
        seq: 1,
        display: crosspane_types::id::DisplayId(1),
        position: crosspane_types::geom::PointDevice::new(1.0, 1.0),
    };
    assert_eq!(link.send_motion(&pointer), Err(LinkError::Closed));
    assert_eq!(link.rtt(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_is_reported_once_on_the_other_side_and_sends_then_fail() {
    let (mut a, mut b) = connected_pair().await;
    let mut a_link = a.transport.link(b.id).unwrap();
    let mut b_link = b.transport.link(a.id).unwrap();

    a_link.close("bye");

    b.expect_closed(a.id, LinkError::Closed).await;
    // The closing side learns of its own close the same way, once.
    a.expect_closed(b.id, LinkError::Closed).await;
    a.expect_quiet(QUIET).await;
    b.expect_quiet(QUIET).await;

    assert_all_sends_closed(a_link.as_mut());
    assert_all_sends_closed(b_link.as_mut());
    assert!(a.transport.peers().is_empty());
    assert!(b.transport.peers().is_empty());
    assert!(a.transport.link(b.id).is_none());
    assert!(b.transport.link(a.id).is_none());

    // Handles from the closed link stay closed even if the peers connect again.
    assert_eq!(a.transport.connect(b.addr()).await.unwrap(), b.id);
    a.expect_hello(b.id, "b").await;
    b.expect_hello(a.id, "a").await;
    assert_all_sends_closed(a_link.as_mut());
    assert!(a.transport.link(b.id).unwrap().send_input(&key(1)).is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_closes_every_connection_and_stops_accepting() {
    let ida = identity();
    let idb = identity();
    let idc = identity();
    let mut a = Node::start("a", ida.clone(), &[&idb, &idc]);
    let mut b = Node::start("b", idb.clone(), &[&ida]);
    let mut c = Node::start("c", idc.clone(), &[&ida]);
    a.transport.connect(b.addr()).await.unwrap();
    a.transport.connect(c.addr()).await.unwrap();
    // A's two hellos (in either order), and one from A at each of B and C.
    a.next().await;
    a.next().await;
    b.expect_hello(a.id, "a").await;
    c.expect_hello(a.id, "a").await;
    let mut a_to_b = a.transport.link(b.id).unwrap();

    a.transport.shutdown("going away").await;

    b.expect_closed(a.id, LinkError::Closed).await;
    c.expect_closed(a.id, LinkError::Closed).await;
    b.expect_quiet(QUIET).await;
    c.expect_quiet(QUIET).await;
    assert_all_sends_closed(a_to_b.as_mut());
    assert!(a.transport.peers().is_empty());

    // A reports its own side closing, once per peer, and nothing else.
    a.next().await;
    a.next().await;
    a.expect_quiet(QUIET).await;

    // It no longer connects out or accepts.
    assert!(matches!(
        a.transport.connect(b.addr()).await,
        Err(TransportError::Connect(_))
    ));
    assert!(b.transport.connect(a.addr()).await.is_err());
    // Shutting down twice is harmless.
    a.transport.shutdown("again").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_a_transport_closes_its_connections() {
    let (a, mut b) = connected_pair().await;
    let a_id = a.id;
    drop(a);

    b.expect_closed(a_id, LinkError::Closed).await;
    b.expect_quiet(QUIET).await;
    assert!(b.transport.peers().is_empty());
    assert!(b.transport.link(a_id).is_none());
}

/// The idle timeout is 10 s and keep-alives go out every 1 s, so a connection with no traffic must
/// still be open after 11 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keep_alives_hold_an_idle_connection_open_past_the_idle_timeout() {
    let (mut a, mut b) = connected_pair().await;
    tokio::time::sleep(Duration::from_secs(11)).await;
    a.expect_quiet(Duration::from_millis(10)).await;
    b.expect_quiet(Duration::from_millis(10)).await;
    assert_eq!(a.transport.peers(), vec![b.id]);
    assert_eq!(b.transport.peers(), vec![a.id]);
    // And it still carries traffic.
    let mut link = a.transport.link(b.id).unwrap();
    link.send_control(&ControlMessage::Ping { t0: 5 }).unwrap();
    assert!(matches!(
        b.next().await,
        crosspane_protocol::link::LinkEvent::Control {
            msg: ControlMessage::Ping { t0: 5 },
            ..
        }
    ));
}

/// Collect what a peer sent until it reports `Closed`: (keys in order, control messages).
async fn collect_until_closed(node: &mut Node) -> (Vec<u32>, Vec<ControlMessage>, LinkEvent) {
    let (mut keys, mut control) = (Vec::new(), Vec::new());
    loop {
        match node.next().await {
            LinkEvent::Input {
                msg: InputMessage::Key { seq, .. },
                ..
            } => keys.push(seq),
            LinkEvent::Control { msg, .. } => control.push(msg),
            closed @ LinkEvent::Closed { .. } => return (keys, control, closed),
            other => panic!("unexpected event {other:?}"),
        }
    }
}

/// What the engine sends just before it closes (key releases, EndControl, Goodbye) must arrive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_flushes_everything_queued_first() {
    let (a, mut b) = connected_pair().await;
    let mut link = a.transport.link(b.id).unwrap();
    for seq in 1..=800 {
        link.send_input(&key(seq)).unwrap();
    }
    for t0 in 1..=50 {
        link.send_control(&ControlMessage::Ping { t0 }).unwrap();
    }
    link.send_control(&ControlMessage::Goodbye {
        message: "done".into(),
    })
    .unwrap();
    link.close("done");
    // Closed at once for sending, even though the flush is still going on.
    assert_eq!(link.send_input(&key(801)), Err(LinkError::Closed));

    let (keys, control, closed) = collect_until_closed(&mut b).await;
    assert_eq!(keys, (1..=800).collect::<Vec<u32>>());
    assert_eq!(
        control.len(),
        51,
        "control messages that arrived: {control:?}"
    );
    assert!(matches!(
        control.last(),
        Some(ControlMessage::Goodbye { message }) if message == "done"
    ));
    assert!(matches!(
        closed,
        LinkEvent::Closed {
            error: LinkError::Closed,
            ..
        }
    ));
    b.expect_quiet(QUIET).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_flushes_everything_queued_first() {
    let (a, mut b) = connected_pair().await;
    let mut link = a.transport.link(b.id).unwrap();
    for seq in 1..=800 {
        link.send_input(&key(seq)).unwrap();
    }
    link.send_control(&ControlMessage::Goodbye {
        message: "going".into(),
    })
    .unwrap();
    a.transport.shutdown("going").await;

    let (keys, control, closed) = collect_until_closed(&mut b).await;
    assert_eq!(keys, (1..=800).collect::<Vec<u32>>());
    assert!(matches!(
        control.as_slice(),
        [ControlMessage::Goodbye { message }] if message == "going"
    ));
    assert!(matches!(closed, LinkEvent::Closed { .. }));
}

/// A panic in the engine's event sink must not leave a half-dead connection behind: the
/// connection closes, the peer is told, and the engine hears `Closed` once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_panicking_sink_cannot_leave_a_zombie_connection() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use crosspane_protocol::link::LinkEventSink;
    use crosspane_transport::{Transport, TransportConfig};
    use tokio::sync::mpsc::unbounded_channel;

    let ida = identity();
    let idb = identity();
    let (tx, mut events_b) = unbounded_channel();
    let exploded = Arc::new(AtomicBool::new(false));
    let sink: LinkEventSink = {
        let exploded = exploded.clone();
        Arc::new(move |event: LinkEvent| {
            if matches!(event, LinkEvent::Input { .. }) && !exploded.swap(true, Ordering::SeqCst) {
                panic!("the engine's sink panicked (on purpose, in a test)");
            }
            let _ = tx.send(event);
        })
    };
    let b = Transport::bind(
        TransportConfig {
            bind: loopback(),
            identity: idb.clone(),
            pins: Pins::of(&[&ida]),
            hello: hello("b"),
        },
        sink,
    )
    .unwrap();
    let mut a = Node::start("a", ida.clone(), &[&idb]);
    a.transport.connect(b.local_addr()).await.unwrap();
    a.expect_hello(idb.node(), "b").await;
    // B's Hello event is in its channel.
    assert!(matches!(
        next_event(&mut events_b).await,
        LinkEvent::Control {
            msg: ControlMessage::Hello(_),
            ..
        }
    ));

    // The first input message makes B's sink panic inside the connection's reader task.
    a.transport
        .link(idb.node())
        .unwrap()
        .send_input(&key(1))
        .unwrap();
    // B reports the closure exactly once, A learns of it, and nothing is left registered.
    assert!(matches!(
        next_event(&mut events_b).await,
        LinkEvent::Closed {
            error: LinkError::Closed,
            ..
        }
    ));
    a.expect_closed(idb.node(), LinkError::Closed).await;
    assert!(
        tokio::time::timeout(QUIET, events_b.recv()).await.is_err(),
        "a second event after Closed"
    );
    assert!(b.peers().is_empty());
    assert!(a.transport.peers().is_empty());
}
