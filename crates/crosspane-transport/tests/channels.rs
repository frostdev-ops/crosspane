#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use common::*;
use crosspane_protocol::link::{LinkError, LinkEvent, PeerLink};
use crosspane_protocol::msg::{ControlMessage, InputMessage, PointerMessage};
use crosspane_types::geom::PointDevice;
use crosspane_types::id::{DisplayId, SessionId};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::sleep;

const MESSAGES: u32 = 10_000;

fn pointer(seq: u32) -> PointerMessage {
    PointerMessage {
        session: SessionId(1),
        seq,
        display: DisplayId(1),
        position: PointDevice::new(f64::from(seq), 2.0 * f64::from(seq)),
    }
}

/// Key messages per 100 ms batch: 1,500 a second, comfortably under the receiver's 2,000-per-second
/// input limit (04 §3), so that a receiver stalled for a few hundred milliseconds on a slow CI
/// machine, then catching up, still stays inside it. Each batch is followed by a sleep.
const BATCH: u32 = 150;

/// Send `MESSAGES` key messages paced below the receiver's input limit. One control message and
/// one pointer datagram go along with each batch.
async fn send_everything(mut link: Box<dyn PeerLink>, tag: u64) {
    let batches = MESSAGES.div_ceil(BATCH);
    for batch in 0..batches {
        for i in 0..BATCH {
            let seq = batch * BATCH + i + 1;
            if seq <= MESSAGES {
                link.send_input(&key(seq)).unwrap();
            }
        }
        link.send_control(&ControlMessage::Ping {
            t0: tag * 1_000_000 + u64::from(batch),
        })
        .unwrap();
        // Datagrams are best effort: a full buffer is congestion, not a failure.
        match link.send_motion(&pointer(batch + 1)) {
            Ok(()) | Err(LinkError::Congested) => {}
            Err(other) => panic!("send_motion failed: {other:?}"),
        }
        sleep(Duration::from_millis(100)).await;
    }
}

struct Received {
    keys: Vec<u32>,
    pings: Vec<u64>,
    pointers: Vec<u32>,
}

/// Collect events until all key messages arrived.
async fn collect(events: &mut UnboundedReceiver<LinkEvent>) -> Received {
    let mut received = Received {
        keys: Vec::new(),
        pings: Vec::new(),
        pointers: Vec::new(),
    };
    while received.keys.len() < MESSAGES as usize {
        match next_event(events).await {
            LinkEvent::Input {
                msg: InputMessage::Key { seq, session, .. },
                ..
            } => {
                assert_eq!(session, SessionId(1));
                received.keys.push(seq);
            }
            LinkEvent::Control {
                msg: ControlMessage::Ping { t0 },
                ..
            } => received.pings.push(t0),
            LinkEvent::Motion { msg, .. } => received.pointers.push(msg.seq),
            other => panic!("unexpected event {other:?}"),
        }
    }
    received
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_control_and_pointer_flow_both_ways() {
    let (mut a, mut b) = connected_pair().await;
    let a_to_b = a.transport.link(b.id).unwrap();
    let b_to_a = b.transport.link(a.id).unwrap();
    let senders = (
        tokio::spawn(send_everything(a_to_b, 1)),
        tokio::spawn(send_everything(b_to_a, 2)),
    );
    let (at_b, at_a) = tokio::join!(collect(&mut b.events), collect(&mut a.events));
    senders.0.await.unwrap();
    senders.1.await.unwrap();

    let in_order: Vec<u32> = (1..=MESSAGES).collect();
    assert_eq!(at_b.keys, in_order, "keys A->B out of order or missing");
    assert_eq!(at_a.keys, in_order, "keys B->A out of order or missing");

    // The rest of the control messages ride their own reliable stream; wait for them.
    for (name, node, received, tag) in [("B", &mut b, at_b, 1u64), ("A", &mut a, at_a, 2u64)] {
        let mut pings = received.pings;
        let mut pointers = received.pointers;
        let batches = usize::try_from(MESSAGES.div_ceil(BATCH)).unwrap();
        while pings.len() < batches {
            match next_event(&mut node.events).await {
                LinkEvent::Control {
                    msg: ControlMessage::Ping { t0 },
                    ..
                } => pings.push(t0),
                LinkEvent::Motion { msg, .. } => pointers.push(msg.seq),
                other => panic!("unexpected event {other:?}"),
            }
        }
        while let Ok(LinkEvent::Motion { msg, .. }) = node.events.try_recv() {
            pointers.push(msg.seq);
        }
        let expected: Vec<u64> = (0..u64::from(MESSAGES.div_ceil(BATCH)))
            .map(|batch| tag * 1_000_000 + batch)
            .collect();
        assert_eq!(pings, expected, "control messages at {name}");
        assert!(!pointers.is_empty(), "no pointer datagram reached {name}");
        eprintln!(
            "{name} received {} of 100 pointer datagrams",
            pointers.len()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_beyond_2000_per_second_is_dropped_but_the_connection_stays_open() {
    let (a, mut b) = connected_pair().await;
    let mut link = a.transport.link(b.id).unwrap();

    for seq in 1..=5_000 {
        link.send_input(&key_down(seq)).unwrap();
    }
    // The first 2,000 arrive within a few milliseconds. Anything past the limit would follow
    // right behind them, so watch a little longer before counting.
    let mut delivered = Vec::new();
    let mut first_at = None;
    let mut last_at = Instant::now();
    while delivered.len() < 1_900 {
        match b.next().await {
            LinkEvent::Input {
                msg: InputMessage::Key { seq, .. },
                ..
            } => {
                last_at = Instant::now();
                first_at.get_or_insert(last_at);
                delivered.push(seq);
            }
            other => panic!("unexpected event {other:?}"),
        }
    }
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(400), b.events.recv()).await
    {
        match event {
            LinkEvent::Input {
                msg: InputMessage::Key { seq, .. },
                ..
            } => {
                last_at = Instant::now();
                delivered.push(seq);
            }
            other => panic!("unexpected event {other:?}"),
        }
    }
    // At most 2,000 in any one second: over a span of S seconds, at most 2,000 * (floor(S) + 1).
    // On a fast machine the whole burst lands within a second, so this is exactly the limit; a slow
    // one that spreads it out is allowed proportionally more, so the test stays valid.
    let span = last_at.duration_since(first_at.unwrap_or(last_at)) + Duration::from_millis(50);
    let allowed = 2_000 * (usize::try_from(span.as_secs()).unwrap() + 1);
    eprintln!(
        "delivered {} of 5000 over {span:?} (allowed {allowed})",
        delivered.len()
    );
    assert!(
        (1_900..=allowed).contains(&delivered.len()),
        "delivered {}",
        delivered.len()
    );
    // The messages that got through are the first ones, in order.
    assert!(delivered.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(delivered[0], 1);

    // The connection is still open, and control is not limited.
    assert_eq!(b.transport.peers(), vec![a.id]);
    link.send_control(&ControlMessage::Ping { t0: 7 }).unwrap();
    assert!(matches!(
        b.next().await,
        LinkEvent::Control {
            msg: ControlMessage::Ping { t0: 7 },
            ..
        }
    ));
    // Once the window has passed, input flows again: keep pressing until one gets through.
    let waiting = Instant::now();
    let mut seq = 9_000;
    loop {
        link.send_input(&key_down(seq)).unwrap();
        seq += 1;
        if let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(100), b.events.recv()).await
        {
            assert!(matches!(event, LinkEvent::Input { .. }), "{event:?}");
            break;
        }
        assert!(
            waiting.elapsed() < Duration::from_secs(4),
            "input never flowed again"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rtt_is_reported_after_traffic() {
    let (mut a, mut b) = connected_pair().await;
    let mut link = a.transport.link(b.id).unwrap();
    for seq in 1..=20 {
        link.send_input(&key(seq)).unwrap();
        link.send_control(&ControlMessage::Ping { t0: u64::from(seq) })
            .unwrap();
    }
    for _ in 0..40 {
        b.next().await;
    }
    let rtt = link.rtt().expect("rtt after traffic");
    assert!(rtt < Duration::from_secs(1), "{rtt:?}");
    assert!(b.transport.link(a.id).unwrap().rtt().is_some());
    a.expect_quiet(Duration::from_millis(100)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn encoding_failures_are_invalid_not_closed() {
    let (a, b) = connected_pair().await;
    let mut link = a.transport.link(b.id).unwrap();
    // A non-finite pointer position cannot be encoded.
    let mut bad = pointer(1);
    bad.position = PointDevice::new(f64::NAN, 0.0);
    assert!(matches!(link.send_motion(&bad), Err(LinkError::Invalid(_))));
    // The link is unaffected.
    link.send_input(&key(1)).unwrap();
}

/// A lost release leaves a key stuck down, and a lost heartbeat looks like a dead link: the rate
/// limit drops excess presses but never these (04 §8).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_rate_limit_never_drops_releases_heartbeats_or_acks() {
    use crosspane_types::hid::MouseButton;

    let (a, mut b) = connected_pair().await;
    let mut link = a.transport.link(b.id).unwrap();
    let session = SessionId(1);

    // Use up the budget with presses, then keep sending: presses are dropped, the rest are not.
    let mut sent_protected = 0;
    for seq in 1..=3_000u32 {
        link.send_input(&key_down(seq)).unwrap();
    }
    for seq in 3_001..=3_300u32 {
        let msg = match seq % 3 {
            0 => InputMessage::Key {
                session,
                seq,
                usage: crosspane_types::hid::HidUsage::keyboard(4),
                down: false,
            },
            1 => InputMessage::Button {
                session,
                seq,
                button: MouseButton::PRIMARY,
                down: false,
            },
            _ => InputMessage::State {
                session,
                seq,
                held_keys: Vec::new(),
                held_buttons: Vec::new(),
            },
        };
        link.send_input(&msg).unwrap();
        sent_protected += 1;
    }
    link.send_input(&InputMessage::Ack {
        session,
        seq: 3_301,
    })
    .unwrap();
    sent_protected += 1;
    // More presses after the protected ones: still over the limit, still dropped.
    for seq in 3_302..=3_400u32 {
        link.send_input(&key_down(seq)).unwrap();
    }

    // Wait for every protected message, then watch a little longer for stray presses.
    let (mut presses, mut protected) = (0, 0);
    let mut count = |event: LinkEvent| match event {
        LinkEvent::Input {
            msg: InputMessage::Key { down: true, .. },
            ..
        } => presses += 1,
        LinkEvent::Input { .. } => protected += 1,
        other => panic!("unexpected event {other:?}"),
    };
    let mut seen = 0;
    while seen < sent_protected {
        let event = b.next().await;
        if !matches!(
            event,
            LinkEvent::Input {
                msg: InputMessage::Key { down: true, .. },
                ..
            }
        ) {
            seen += 1;
        }
        count(event);
    }
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(300), b.events.recv()).await
    {
        count(event);
    }
    assert_eq!(protected, sent_protected, "a protected message was dropped");
    assert!(
        (1_600..=2_000).contains(&presses),
        "presses delivered: {presses}"
    );
}
