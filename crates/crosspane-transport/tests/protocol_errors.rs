//! A peer that breaks the protocol is disconnected with code 1, and the engine hears `Closed`
//! with `LinkError::Invalid`. The misbehaving side is a raw QUIC client authenticated as a pinned
//! peer.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::{ControlMessage, InputMessage, Placement};
use crosspane_protocol::wire::{
    FrameDecoder, KIND_CONTROL, KIND_KEY, MAX_CONTROL_PAYLOAD, MAX_INPUT_PAYLOAD, WIRE_VERSION,
    decode_control, encode_control,
};
use crosspane_types::id::NodeId;

const QUIET: Duration = Duration::from_millis(400);

struct Rig {
    node: Node,
    peer: NodeId,
    conn: quinn::Connection,
}

/// A transport that pins a raw client, with the client connected.
async fn rig() -> Rig {
    let node_id = identity();
    let raw_id = identity();
    let node = Node::start("node", node_id.clone(), &[&raw_id]);
    let raw = Raw::new();
    let conn = raw.connect(&raw_id, &node_id, node.addr()).await;
    Rig {
        peer: raw_id.node(),
        node,
        conn,
    }
}

impl Rig {
    /// Complete the handshake properly: Hello on the control stream.
    async fn say_hello(&mut self) -> quinn::SendStream {
        let control = open_stream(&self.conn, 0x01, &hello_frame("raw")).await;
        self.node.expect_hello(self.peer, "raw").await;
        control
    }

    /// The node closed the connection for a protocol error and told its engine.
    async fn expect_protocol_error(&mut self) {
        self.expect_protocol_error_within(WAIT).await;
    }

    async fn expect_protocol_error_within(&mut self, wait: Duration) {
        let event = tokio::time::timeout(wait, self.node.events.recv())
            .await
            .expect("timed out waiting for the protocol error")
            .expect("the event sink was dropped");
        match event {
            LinkEvent::Closed {
                peer,
                error: LinkError::Invalid(why),
            } => {
                assert_eq!(peer, self.peer);
                eprintln!("protocol error reported: {why}");
                assert!(!why.is_empty());
            }
            other => panic!("expected Closed with Invalid, got {other:?}"),
        }
        let (code, reason) = closed_by_peer(&self.conn).await;
        assert_eq!((code, reason.as_str()), (1, "protocol error"));
        self.node.expect_quiet(QUIET).await;
        assert!(self.node.transport.peers().is_empty());
    }
}

fn header(kind: u8, len: u32) -> Vec<u8> {
    let mut bytes = vec![WIRE_VERSION, kind, 0, 0];
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn garbage_on_the_control_stream_closes_the_connection() {
    let mut rig = rig().await;
    let _stream = open_stream(&rig.conn, 0x01, &[0xff; 64]).await;
    rig.expect_protocol_error().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_oversized_control_frame_closes_the_connection() {
    let mut rig = rig().await;
    let len = u32::try_from(MAX_CONTROL_PAYLOAD + 1).unwrap();
    let _stream = open_stream(&rig.conn, 0x01, &header(KIND_CONTROL, len)).await;
    rig.expect_protocol_error().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_oversized_input_frame_closes_the_connection() {
    let mut rig = rig().await;
    let _control = rig.say_hello().await;
    let len = u32::try_from(MAX_INPUT_PAYLOAD + 1).unwrap();
    let _input = open_stream(&rig.conn, 0x02, &header(KIND_KEY, len)).await;
    rig.expect_protocol_error().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_malformed_input_message_closes_the_connection() {
    let mut rig = rig().await;
    let _control = rig.say_hello().await;
    // A key message with a payload of the wrong length.
    let mut frame = header(KIND_KEY, 3);
    frame.extend_from_slice(&[1, 2, 3]);
    let _input = open_stream(&rig.conn, 0x02, &frame).await;
    rig.expect_protocol_error().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_stream_type_closes_the_connection() {
    let mut rig = rig().await;
    let _stream = open_stream(&rig.conn, 0x7f, &[]).await;
    rig.expect_protocol_error().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_control_stream_closes_the_connection() {
    let mut rig = rig().await;
    let _first = rig.say_hello().await;
    let _second = open_stream(&rig.conn, 0x01, &[]).await;
    rig.expect_protocol_error().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_input_stream_closes_the_connection() {
    let mut rig = rig().await;
    let _control = rig.say_hello().await;
    let _first = open_stream(&rig.conn, 0x02, &[]).await;
    let _second = open_stream(&rig.conn, 0x02, &[]).await;
    rig.expect_protocol_error().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_first_control_message_other_than_hello_closes_the_connection() {
    let mut rig = rig().await;
    let mut frame = Vec::new();
    encode_control(&ControlMessage::Ping { t0: 1 }, &mut frame).unwrap();
    let _stream = open_stream(&rig.conn, 0x01, &frame).await;
    rig.expect_protocol_error().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_malformed_pointer_datagram_closes_the_connection() {
    let mut rig = rig().await;
    let _control = rig.say_hello().await;
    rig.conn.send_datagram(vec![0xee; 32].into()).unwrap();
    rig.expect_protocol_error().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_control_messages_are_skipped() {
    let mut rig = rig().await;
    // A control frame whose body this version doesn't know (here: an empty message) is ignored,
    // before and after the Hello.
    let empty = header(KIND_CONTROL, 0);
    let mut control = open_stream(&rig.conn, 0x01, &empty).await;
    control.write_all(&hello_frame("raw")).await.unwrap();
    control.write_all(&empty).await.unwrap();
    let mut ping = Vec::new();
    encode_control(&ControlMessage::Ping { t0: 9 }, &mut ping).unwrap();
    control.write_all(&ping).await.unwrap();

    rig.node.expect_hello(rig.peer, "raw").await;
    assert!(matches!(
        rig.node.next().await,
        LinkEvent::Control {
            msg: ControlMessage::Ping { t0: 9 },
            ..
        }
    ));
    rig.node.expect_quiet(QUIET).await;
    assert_eq!(rig.node.transport.peers(), vec![rig.peer]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_well_behaved_raw_peer_is_served() {
    let mut rig = rig().await;
    let _control = rig.say_hello().await;
    let mut input = open_stream(&rig.conn, 0x02, &[]).await;
    for seq in 1..=50 {
        input.write_all(&key_frame(seq)).await.unwrap();
    }
    for seq in 1..=50 {
        match rig.node.next().await {
            LinkEvent::Input {
                peer,
                msg: InputMessage::Key { seq: got, .. },
            } => {
                assert_eq!(peer, rig.peer);
                assert_eq!(got, seq);
            }
            other => panic!("unexpected event {other:?}"),
        }
    }
    // The node's own two streams reach the raw side: a type byte each, and a Hello first on the
    // control stream.
    let mut streams = Vec::new();
    for _ in 0..2 {
        let mut recv = tokio::time::timeout(WAIT, rig.conn.accept_uni())
            .await
            .unwrap()
            .unwrap();
        let mut kind = [0u8; 1];
        recv.read_exact(&mut kind).await.unwrap();
        streams.push((kind[0], recv));
    }
    streams.sort_by_key(|(kind, _)| *kind);
    let kinds: Vec<u8> = streams.iter().map(|(kind, _)| *kind).collect();
    assert_eq!(kinds, vec![0x01, 0x02]);
    let control = &mut streams[0].1;
    let mut decoder = FrameDecoder::new(MAX_CONTROL_PAYLOAD);
    let frame = loop {
        if let Some(frame) = decoder.next_frame().unwrap() {
            break frame;
        }
        let chunk = tokio::time::timeout(WAIT, control.read_chunk(4096, true))
            .await
            .unwrap()
            .unwrap()
            .expect("the control stream ended");
        decoder.push(&chunk.bytes);
    };
    match decode_control(&frame).unwrap() {
        ControlMessage::Hello(hello) => assert_eq!(hello.name, "node"),
        other => panic!("expected a hello first, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_hello_closes_the_connection() {
    let mut rig = rig().await;
    let mut control = rig.say_hello().await;
    // The engine is promised exactly one Hello; a repeat is never forwarded.
    control.write_all(&hello_frame("again")).await.unwrap();
    rig.expect_protocol_error().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_hello_within_five_seconds_closes_the_connection() {
    let mut rig = rig().await;
    // Both streams open, but the peer never says Hello.
    let _control = open_stream(&rig.conn, 0x01, &[]).await;
    let _input = open_stream(&rig.conn, 0x02, &[]).await;
    let started = std::time::Instant::now();
    rig.expect_protocol_error_within(Duration::from_secs(15))
        .await;
    // The deadline is 5 s from the handshake; `started` is a little later than that.
    assert!(
        started.elapsed() >= Duration::from_secs(3),
        "closed after only {:?}",
        started.elapsed()
    );
}

/// If the peer refuses one of our streams (STOP_SENDING) the link is half dead: the connection
/// must close rather than silently drop what the engine sends next.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_the_peer_refuses_closes_the_connection() {
    let mut rig = rig().await;
    let _control = rig.say_hello().await;
    let mut streams = Vec::new();
    for _ in 0..2 {
        let mut recv = tokio::time::timeout(WAIT, rig.conn.accept_uni())
            .await
            .unwrap()
            .unwrap();
        let mut kind = [0u8; 1];
        recv.read_exact(&mut kind).await.unwrap();
        streams.push((kind[0], recv));
    }
    // Refuse the node's control stream.
    let (_, control) = streams.iter_mut().find(|(kind, _)| *kind == 0x01).unwrap();
    control.stop(7u32.into()).unwrap();

    let mut link = rig.node.transport.link(rig.peer).unwrap();
    let mut closed = false;
    for t0 in 0..100 {
        if link.send_control(&ControlMessage::Ping { t0 }) == Err(LinkError::Closed) {
            closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(closed, "sends kept succeeding on a refused stream");
    let (code, reason) = closed_by_peer(&rig.conn).await;
    assert_eq!((code, reason.as_str()), (1, "stream error"));
    assert!(matches!(
        rig.node.next().await,
        LinkEvent::Closed {
            error: LinkError::Closed,
            ..
        }
    ));
    rig.node.expect_quiet(QUIET).await;
}

/// A peer that stops reading must not make the send queue grow without bound: past its cap the
/// connection closes (code 3) and sends fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_that_stops_reading_loses_its_connection_instead_of_growing_the_queue() {
    use crosspane_types::geom::PointMm;
    use crosspane_types::id::DisplayId;

    let mut rig = rig().await;
    let _control = rig.say_hello().await;
    // The raw peer never reads the node's streams.
    let mut link = rig.node.transport.link(rig.peer).unwrap();
    let layout = ControlMessage::Layout(
        (0..64)
            .map(|i| Placement {
                node: rig.peer,
                display: DisplayId(i),
                origin: PointMm::new(0.0, 0.0),
                version: 1,
            })
            .collect(),
    );
    let mut refused_after = None;
    for sent in 0..20_000 {
        match link.send_control(&layout) {
            Ok(()) => {}
            Err(LinkError::Closed) => {
                refused_after = Some(sent);
                break;
            }
            Err(other) => panic!("unexpected {other:?}"),
        }
    }
    let sent = refused_after.expect("the queue never hit its cap");
    eprintln!("queue cap reached after {sent} layout messages");
    assert!(sent > 100, "closed far too early: {sent}");

    let (code, reason) = closed_by_peer(&rig.conn).await;
    assert_eq!((code, reason.as_str()), (3, "send queue overflow"));
    assert!(matches!(
        rig.node.next().await,
        LinkEvent::Closed {
            error: LinkError::Closed,
            ..
        }
    ));
}
