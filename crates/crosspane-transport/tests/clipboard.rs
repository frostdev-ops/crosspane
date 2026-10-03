//! Clipboard fixtures use only owned loopback transports and synthetic bytes.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use crosspane_protocol::clip::{
    ClipDataHeader, MAX_CLIP_IMAGE, MAX_CLIP_TEXT, encode_clip_data_header,
};
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::{Capability, ClipFetchId, ControlMessage};
use crosspane_types::ClipKind;
use crosspane_types::id::NodeId;
use quinn::{Connection, SendStream};
use tokio::time::{sleep, timeout};

const FEATURES: &[&str] = &["e1", "clip/0"];
const QUIET: Duration = Duration::from_millis(100);

fn header(fetch: u64, kind: ClipKind, len: usize) -> ClipDataHeader {
    ClipDataHeader {
        fetch: ClipFetchId(fetch),
        kind,
        len: len.try_into().unwrap(),
    }
}

async fn pair(features_a: &[&str], features_b: &[&str]) -> (Node, Node) {
    let ia = identity();
    let ib = identity();
    let mut a = node_with_hello(ia.clone(), &[&ib], hello_with("a", features_a));
    let mut b = node_with_hello(ib, &[&ia], hello_with("b", features_b));
    a.transport.connect(b.addr()).await.unwrap();
    a.expect_hello(b.id, "b").await;
    b.expect_hello(a.id, "a").await;
    (a, b)
}

async fn expect_data(node: &mut Node, peer: NodeId, fetch: u64, kind: ClipKind, bytes: &[u8]) {
    match node.next().await {
        LinkEvent::ClipData {
            peer: from,
            fetch: id,
            kind: got_kind,
            data,
        } => {
            assert_eq!((from, id, got_kind), (peer, ClipFetchId(fetch), kind));
            // Deliberately avoid content-bearing assertion diagnostics.
            assert!(data.0.as_ref() == bytes, "clipboard fixture changed");
        }
        other => panic!("expected clipboard data, got {other:?}"),
    }
}

struct Fixture {
    node: Node,
    peer: NodeId,
    conn: Connection,
    control: SendStream,
    _input: SendStream,
    _raw: Raw,
}

impl Fixture {
    async fn new(local: &[&str], remote: &[&str]) -> Self {
        Self::with_transport(local, remote, None).await
    }

    async fn with_transport(
        local: &[&str],
        remote: &[&str],
        transport: Option<quinn::TransportConfig>,
    ) -> Self {
        let ia = identity();
        let ir = identity();
        let mut node = node_with_hello(ia.clone(), &[&ir], hello_with("node", local));
        let raw = Raw::new();
        let conn = match transport {
            Some(transport) => raw
                .connect_with_transport(&ir, &ia, node.addr(), crosspane_protocol::ALPN, transport)
                .await
                .unwrap(),
            None => raw.connect(&ir, &ia, node.addr()).await,
        };
        let control = open_stream(&conn, 1, &hello_frame_with("raw", remote)).await;
        let input = open_stream(&conn, 2, &[]).await;
        node.expect_hello(ir.node(), "raw").await;
        Self {
            node,
            peer: ir.node(),
            conn,
            control,
            _input: input,
            _raw: raw,
        }
    }

    fn expect(&self, fetch: u64, kind: ClipKind) {
        self.node
            .transport
            .expect_clip(self.peer, ClipFetchId(fetch), kind)
            .unwrap();
    }

    async fn stream(&self, raw_header: &[u8], bytes: &[u8], fin: bool) -> SendStream {
        let mut send = open_stream(&self.conn, 4, raw_header).await;
        send.write_all(bytes).await.unwrap();
        if fin {
            send.finish().unwrap();
        }
        send
    }

    async fn ping(&mut self) {
        self.control
            .write_all(&control_frame(&ControlMessage::Ping { t0: 77 }))
            .await
            .unwrap();
        assert!(matches!(self.node.next().await,
            LinkEvent::Control { peer, msg: ControlMessage::Ping { t0: 77 } } if peer == self.peer));
    }
}

#[tokio::test]
async fn large_backpressured_clipboard_and_media_keep_input_control_below_500ms() {
    bulk_latency(64 * 1024, false).await;
}

#[tokio::test]
async fn production_stream_credit_with_aggregate_bulk_pending_keeps_input_control_below_500ms() {
    bulk_latency(64 * 1024 * 1024, true).await;
}

async fn bulk_latency(stream_credit: u32, aggregate: bool) {
    let mut config = quinn::TransportConfig::default();
    config.stream_receive_window(quinn::VarInt::from_u32(stream_credit));
    config.receive_window(quinn::VarInt::from_u32(112 * 1024 * 1024));
    let f = Fixture::with_transport(FEATURES, FEATURES, Some(config)).await;
    let (mut receiver, _) = RawReceiver::accept(&f.conn).await;
    f.node
        .transport
        .send_clip_data(
            f.peer,
            header(1, ClipKind::Image, MAX_CLIP_IMAGE as usize),
            vec![0x5a; MAX_CLIP_IMAGE as usize].into(),
        )
        .unwrap();
    f.node
        .transport
        .send_media(f.peer, vec![0x3a; 64 * 1024 * 1024].into())
        .unwrap();
    let before = f.conn.stats().udp_rx.bytes;
    // Hold both bulk streams unread. The production-credit case also keeps the tag unread:
    // a 64 MiB media payload plus its tag cannot finish within 64 MiB of stream credit.
    let mut held = Vec::new();
    let mut tags = Vec::new();
    for _ in 0..2 {
        let mut recv = timeout(WAIT, f.conn.accept_uni()).await.unwrap().unwrap();
        if !aggregate {
            let mut tag = [0];
            recv.read_exact(&mut tag).await.unwrap();
            tags.push(tag[0]);
        }
        held.push(recv);
    }
    if aggregate {
        // More than 8 MiB of owned QUIC traffic must arrive while the bulk streams stay unread.
        // This cannot be met by the old 64 KiB per-stream pressure fixture.
        timeout(Duration::from_secs(1), async {
            while f.conn.stats().udp_rx.bytes - before < 8 * 1024 * 1024 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("aggregate bulk traffic did not reach the production-credit fixture");
    } else {
        tags.sort_unstable();
        assert_eq!(tags, [3, 4]);
    }
    assert_eq!(
        f.node
            .transport
            .send_clip_data(f.peer, header(2, ClipKind::Text, 1), Arc::from([0x5a])),
        Err(LinkError::Congested)
    );
    assert_eq!(
        f.node.transport.send_media(f.peer, Arc::from([0x3a])),
        Err(LinkError::Congested)
    );
    let mut link = f.node.transport.link(f.peer).unwrap();
    let started = tokio::time::Instant::now();
    link.send_control(&ControlMessage::Ping { t0: 123 })
        .unwrap();
    link.send_input(&key(1)).unwrap();
    timeout(Duration::from_millis(500), async {
        assert_eq!(receiver.next().await, ControlMessage::Ping { t0: 123 });
        assert_eq!(receiver.next_input().await, key(1));
    })
    .await
    .expect("input/control stalled behind owned clipboard/media backpressure");
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(
        f.node
            .transport
            .send_clip_data(f.peer, header(3, ClipKind::Text, 1), Arc::from([0x5a])),
        Err(LinkError::Congested)
    );
    assert_eq!(
        f.node.transport.send_media(f.peer, Arc::from([0x3a])),
        Err(LinkError::Congested)
    );
    f.node.transport.cancel_clip(f.peer);
    f.conn
        .close(quinn::VarInt::from_u32(0), b"fixture complete");
    drop(held);
}

async fn reset(send: &SendStream) {
    let stopped = timeout(WAIT, send.stopped()).await.unwrap().unwrap();
    assert_eq!(stopped, Some(quinn::VarInt::from_u32(4)));
}

async fn rejected_finished(send: &SendStream) {
    // Quinn's stopped() returns None once FIN is ACKed: a later STOP is no longer observable.
    // The caller also checks no data event escapes and the independent control stream works.
    let stopped = timeout(WAIT, send.stopped()).await.unwrap().unwrap();
    assert!(stopped.is_none() || stopped == Some(quinn::VarInt::from_u32(4)));
}

#[tokio::test]
async fn exact_clipboard_streams_round_trip_both_kinds_caps_and_zero_length() {
    let (mut a, mut b) = pair(FEATURES, FEATURES).await;
    for (fetch, kind, len) in [
        (1, ClipKind::Text, MAX_CLIP_TEXT as usize),
        (2, ClipKind::Image, MAX_CLIP_IMAGE as usize),
        (3, ClipKind::Text, 0),
    ] {
        let bytes: Arc<[u8]> = vec![0x5a; len].into();
        b.transport
            .expect_clip(a.id, ClipFetchId(fetch), kind)
            .unwrap();
        wait_until("clipboard send credit after cancellation", || {
            match a
                .transport
                .send_clip_data(b.id, header(fetch, kind, len), bytes.clone())
            {
                Ok(()) => true,
                Err(LinkError::Congested) => false,
                Err(error) => panic!("clipboard send failed: {error:?}"),
            }
        })
        .await;
        let ai = a.id;
        expect_data(&mut b, ai, fetch, kind, &bytes).await;
        // Each cap fixture is complete. Retire its send credit rather than spending a new
        // expectation's two-second lifetime waiting behind the previous fixed expiry.
        a.transport.cancel_clip(b.id);
    }
    a.transport
        .expect_clip(b.id, ClipFetchId(4), ClipKind::Text)
        .unwrap();
    b.transport
        .send_clip_data(a.id, header(4, ClipKind::Text, 3), Arc::from(&b"own"[..]))
        .unwrap();
    let bi = b.id;
    expect_data(&mut a, bi, 4, ClipKind::Text, b"own").await;
}

#[tokio::test]
async fn either_missing_clip_feature_filters_grants_and_refuses_work() {
    for (local, remote) in [
        (FEATURES, &["e1"][..]),
        (&["e1"][..], FEATURES),
        (&["e1"][..], &["e1"][..]),
    ] {
        let (a, mut b) = pair(local, remote).await;
        for (source, target) in [(&a, &b), (&b, &a)] {
            assert_eq!(
                source
                    .transport
                    .expect_clip(target.id, ClipFetchId(1), ClipKind::Text),
                Err(LinkError::Invalid("clipboard is unavailable"))
            );
            assert_eq!(
                source.transport.send_clip_data(
                    target.id,
                    header(1, ClipKind::Text, 0),
                    Arc::from([])
                ),
                Err(LinkError::Invalid("clipboard is unavailable"))
            );
        }
        a.transport
            .link(b.id)
            .unwrap()
            .send_control(&ControlMessage::Grants(vec![
                Capability::InputAccept,
                Capability::ClipboardRead,
                Capability::WindowShare,
                Capability::ClipboardWrite,
            ]))
            .unwrap();
        assert!(
            matches!(b.next().await, LinkEvent::Control { msg: ControlMessage::Grants(grants), .. }
            if grants == [Capability::InputAccept, Capability::WindowShare])
        );
    }
    let (a, mut b) = pair(FEATURES, FEATURES).await;
    a.transport
        .link(b.id)
        .unwrap()
        .send_control(&ControlMessage::Grants(vec![
            Capability::ClipboardRead,
            Capability::ClipboardWrite,
        ]))
        .unwrap();
    assert!(
        matches!(b.next().await, LinkEvent::Control { msg: ControlMessage::Grants(grants), .. }
        if grants == [Capability::ClipboardRead, Capability::ClipboardWrite])
    );
}

#[tokio::test]
async fn malformed_unexpected_and_inexact_streams_are_rejected_without_harming_control() {
    let mut f = Fixture::new(FEATURES, FEATURES).await;
    for case in 0..9u64 {
        let mut raw = encode_clip_data_header(header(case, ClipKind::Text, 2))
            .unwrap()
            .to_vec();
        let mut bytes = vec![0x5a; 2];
        if case != 0 {
            f.expect(case, ClipKind::Text);
        }
        match case {
            0 => {}          // No expected fetch.
            1 => raw[8] = 2, // Expected Text, received Image.
            2 => raw[8] = 3, // Unknown kind.
            3 => raw[9..].copy_from_slice(&(MAX_CLIP_TEXT + 1).to_le_bytes()),
            4 => {
                raw[8] = 2;
                raw[9..].copy_from_slice(&(MAX_CLIP_IMAGE + 1).to_le_bytes());
            }
            5 => {
                raw.pop();
            }
            6 => {
                bytes.pop();
            }
            7 => bytes.push(0x5a),
            8 => bytes.clear(),
            _ => unreachable!(),
        }
        let truncated = matches!(case, 5 | 6 | 8);
        let send = f.stream(&raw, &bytes, truncated).await;
        if truncated {
            rejected_finished(&send).await;
        } else {
            reset(&send).await;
        }
        f.ping().await;
        f.node.transport.cancel_clip(f.peer);
    }
}

#[tokio::test]
async fn missing_fin_times_out_and_cancel_resets_inflight_data() {
    let mut f = Fixture::new(FEATURES, FEATURES).await;
    f.expect(1, ClipKind::Text);
    let raw = encode_clip_data_header(header(1, ClipKind::Text, 2)).unwrap();
    let send = f.stream(&raw, &[0x5a; 2], false).await;
    reset(&send).await;
    f.ping().await;
    f.expect(2, ClipKind::Text);
    let raw = encode_clip_data_header(header(2, ClipKind::Text, 2)).unwrap();
    let send = f.stream(&raw, &[0x5a], false).await;
    f.node.transport.cancel_clip(f.peer);
    reset(&send).await;
    f.ping().await;
    f.expect(3, ClipKind::Text);
    let raw = encode_clip_data_header(header(3, ClipKind::Text, 2)).unwrap();
    let _send = f.stream(&raw, &[0x5a; 2], true).await;
    let peer = f.peer;
    expect_data(&mut f.node, peer, 3, ClipKind::Text, &[0x5a; 2]).await;
}

#[tokio::test]
async fn expired_duplicate_and_cancelled_expectations_reset() {
    let mut f = Fixture::new(FEATURES, FEATURES).await;
    f.expect(1, ClipKind::Text);
    sleep(Duration::from_millis(2100)).await;
    let raw = encode_clip_data_header(header(1, ClipKind::Text, 0)).unwrap();
    reset(&f.stream(&raw, &[], false).await).await;
    f.expect(2, ClipKind::Text);
    f.node.transport.cancel_clip(f.peer);
    let raw = encode_clip_data_header(header(2, ClipKind::Text, 0)).unwrap();
    reset(&f.stream(&raw, &[], false).await).await;
    f.expect(3, ClipKind::Text);
    let raw = encode_clip_data_header(header(3, ClipKind::Text, 0)).unwrap();
    let _send = f.stream(&raw, &[], true).await;
    let peer = f.peer;
    expect_data(&mut f.node, peer, 3, ClipKind::Text, &[]).await;
    reset(&f.stream(&raw, &[], false).await).await;
    f.ping().await;
}

#[tokio::test]
async fn raw_clipboard_before_hello_or_without_negotiation_resets() {
    let ia = identity();
    let ir = identity();
    let mut node = node_with_hello(ia.clone(), &[&ir], hello_with("node", FEATURES));
    let raw = Raw::new();
    let conn = raw.connect(&ir, &ia, node.addr()).await;
    wait_until("provisional link", || {
        node.transport.link(ir.node()).is_some()
    })
    .await;
    assert!(matches!(
        node.transport
            .expect_clip(ir.node(), ClipFetchId(1), ClipKind::Text),
        Err(LinkError::Invalid(_))
    ));
    let bytes = encode_clip_data_header(header(1, ClipKind::Text, 0)).unwrap();
    let send = open_stream(&conn, 4, &bytes).await;
    reset(&send).await;
    let _control = open_stream(&conn, 1, &hello_frame_with("raw", FEATURES)).await;
    let _input = open_stream(&conn, 2, &[]).await;
    node.expect_hello(ir.node(), "raw").await;
    for (local, remote) in [(FEATURES, &["e1"][..]), (&["e1"][..], FEATURES)] {
        let mut f = Fixture::new(local, remote).await;
        reset(&f.stream(&bytes, &[], false).await).await;
        f.ping().await;
    }
}

#[tokio::test]
async fn expectations_are_bound_to_authenticated_peer() {
    let ia = identity();
    let ib = identity();
    let ic = identity();
    let mut a = node_with_hello(ia.clone(), &[&ib, &ic], hello_with("a", FEATURES));
    let mut b = node_with_hello(ib, &[&ia], hello_with("b", FEATURES));
    let mut c = node_with_hello(ic, &[&ia], hello_with("c", FEATURES));
    let bi = b.id;
    for target in [&mut b, &mut c] {
        a.transport.connect(target.addr()).await.unwrap();
        a.expect_hello(target.id, if target.id == bi { "b" } else { "c" })
            .await;
        target.expect_hello(a.id, "a").await;
    }
    a.transport
        .expect_clip(b.id, ClipFetchId(1), ClipKind::Text)
        .unwrap();
    c.transport
        .send_clip_data(a.id, header(1, ClipKind::Text, 1), Arc::from([0x5a]))
        .unwrap();
    a.expect_quiet(QUIET).await;
    b.transport
        .send_clip_data(a.id, header(1, ClipKind::Text, 1), Arc::from([0x5a]))
        .unwrap();
    expect_data(&mut a, bi, 1, ClipKind::Text, &[0x5a]).await;
}

#[tokio::test]
async fn connection_replacement_retires_expectations_even_with_same_feature() {
    let one = identity();
    let two = identity();
    let (ia, ib) = if one.node() > two.node() {
        (one, two)
    } else {
        (two, one)
    };
    let mut a = node_with_hello(ia.clone(), &[&ib], hello_with("a", FEATURES));
    let mut b1 = node_with_hello(ib.clone(), &[&ia], hello_with("b1", FEATURES));
    let mut b2 = node_with_hello(ib, &[&ia], hello_with("b2", FEATURES));
    a.transport.connect(b1.addr()).await.unwrap();
    a.expect_hello(b1.id, "b1").await;
    b1.expect_hello(a.id, "a").await;
    a.transport
        .expect_clip(b1.id, ClipFetchId(1), ClipKind::Text)
        .unwrap();
    b2.transport.connect(a.addr()).await.unwrap();
    b2.expect_hello(a.id, "a").await;
    assert!(
        matches!(a.next().await, LinkEvent::HelloRefresh { peer, hello } if peer == b2.id && hello.name == "b2")
    );
    b2.transport
        .send_clip_data(a.id, header(1, ClipKind::Text, 1), Arc::from([0x5a]))
        .unwrap();
    a.expect_quiet(QUIET).await;
    a.transport
        .expect_clip(b2.id, ClipFetchId(2), ClipKind::Text)
        .unwrap();
    b2.transport
        .send_clip_data(a.id, header(2, ClipKind::Text, 1), Arc::from([0x5a]))
        .unwrap();
    let bi = b2.id;
    expect_data(&mut a, bi, 2, ClipKind::Text, &[0x5a]).await;
}

#[tokio::test]
async fn clip_send_limits_are_synchronous_and_cancel_recovers_credit() {
    let f = Fixture::new(FEATURES, FEATURES).await;
    let data: Arc<[u8]> = vec![0x5a; MAX_CLIP_IMAGE as usize].into();
    f.node
        .transport
        .send_clip_data(f.peer, header(1, ClipKind::Image, data.len()), data.clone())
        .unwrap();
    assert_eq!(
        f.node
            .transport
            .send_clip_data(f.peer, header(2, ClipKind::Text, 1), Arc::from([0x5a])),
        Err(LinkError::Congested)
    );
    f.node.transport.cancel_clip(f.peer);
    // The cancellation is asynchronous, so poll the bounded synchronous admission, not sleeps.
    wait_until("cancel returns clipboard send credit", || {
        f.node
            .transport
            .send_clip_data(f.peer, header(3, ClipKind::Image, data.len()), data.clone())
            .is_ok()
    })
    .await;
    f.node.transport.cancel_clip(f.peer);
}

#[tokio::test]
async fn clipboard_coexists_with_media_control_input_and_motion() {
    use crosspane_protocol::msg::PointerMessage;
    use crosspane_types::geom::PointDevice;
    use crosspane_types::id::{DisplayId, SessionId};
    let (a, mut b) = pair(FEATURES, FEATURES).await;
    b.transport
        .expect_clip(a.id, ClipFetchId(1), ClipKind::Text)
        .unwrap();
    a.transport
        .send_clip_data(b.id, header(1, ClipKind::Text, 1), Arc::from([0x5a]))
        .unwrap();
    let media: Arc<[u8]> = vec![0x3a; 64].into();
    a.transport.send_media(b.id, media.clone()).unwrap();
    let pointer = PointerMessage {
        session: SessionId(1),
        seq: 2,
        display: DisplayId(1),
        position: PointDevice::new(2.0, 3.0),
    };
    let mut link = a.transport.link(b.id).unwrap();
    link.send_control(&ControlMessage::Ping { t0: 99 }).unwrap();
    link.send_input(&key(1)).unwrap();
    link.send_motion(&pointer).unwrap();
    let mut seen = [false; 5];
    while !seen.iter().all(|yes| *yes) {
        let index = match b.next().await {
            LinkEvent::ClipData {
                peer,
                fetch,
                kind,
                data,
            } => {
                assert_eq!((peer, fetch, kind), (a.id, ClipFetchId(1), ClipKind::Text));
                assert!(data.0.as_ref() == [0x5a]);
                0
            }
            LinkEvent::Media { peer, data } => {
                assert_eq!(peer, a.id);
                assert!(data.as_ref() == media.as_ref());
                1
            }
            LinkEvent::Control {
                msg: ControlMessage::Ping { t0: 99 },
                ..
            } => 2,
            LinkEvent::Input { msg, .. } => {
                assert_eq!(msg, key(1));
                3
            }
            LinkEvent::Motion { msg, .. } => {
                assert_eq!(msg, pointer);
                4
            }
            other => panic!("unexpected event: {other:?}"),
        };
        assert!(!seen[index]);
        seen[index] = true;
    }
}
