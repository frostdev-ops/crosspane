//! E2 media frames (WP-2.4): one unidirectional stream per frame, backpressure instead of queues,
//! and no interference with control and input. Real UDP on loopback.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use crosspane_media::tiles::{TileDecoder, TileEncoder};
use crosspane_media::wire::{FrameHeader, read_header};
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::ControlMessage;
use crosspane_transport::Transport;
use crosspane_types::id::NodeId;
use tokio::time::{sleep, timeout};

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
/// The CPF1 header length: the part of a frame this file builds with the real encoder.
const HEADER: usize = 48;

// ---- frames ------------------------------------------------------------------------------------

/// The first 48 bytes of a real CPF1 frame for `projection`, made by the real encoder so the
/// offsets the transport reads are the format's own.
fn cpf1_header(projection: u64, seq: u64, key: bool) -> Vec<u8> {
    let mut encoder = TileEncoder::new();
    let mut pixels = vec![0u8; 64 * 64 * 4];
    let header = FrameHeader {
        projection,
        seq,
        key,
        captured_ns: 0,
        width: 64,
        height: 64,
    };
    let mut out = Vec::new();
    encoder
        .encode(header, &pixels, 64 * 4, false, &mut out)
        .unwrap();
    if !key {
        // The first frame is always a key frame; a delta needs a change after it.
        pixels[0] = 1;
        encoder
            .encode(header, &pixels, 64 * 4, false, &mut out)
            .unwrap();
    }
    let parsed = read_header(&out).unwrap();
    assert_eq!((parsed.projection, parsed.seq, parsed.key), (projection, seq, key));
    out.truncate(HEADER);
    out
}

/// A frame of exactly `len` bytes (at least `HEADER`): a real header, then position-dependent
/// filler derived from `seq`, so truncation, reordering and mixing of frames are all visible.
fn frame(projection: u64, seq: u64, key: bool, len: usize) -> Arc<[u8]> {
    assert!(len >= HEADER);
    let mut bytes = cpf1_header(projection, seq, key);
    bytes.resize(len, 0);
    for (index, word) in bytes[HEADER..].chunks_mut(8).enumerate() {
        let value = (seq + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15)
            ^ (index as u64).wrapping_mul(0xc2b2_ae3d_27d4_eb4f);
        let value = value.to_le_bytes();
        word.copy_from_slice(&value[..word.len()]);
    }
    Arc::from(bytes)
}

/// A delta frame of `len` bytes on projection `projection`.
fn delta(projection: u64, len: usize) -> Arc<[u8]> {
    frame(projection, 1, false, len)
}

/// A key frame of `len` bytes on projection `projection`.
fn key_frame(projection: u64, len: usize) -> Arc<[u8]> {
    frame(projection, 1, true, len)
}

/// Frame number `index` of the random-size run: its projection, kind and length follow from the
/// index alone, so the receiver can rebuild what it should have got.
fn random_frame(index: u64) -> Arc<[u8]> {
    let mix = (index + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let mix = (mix ^ (mix >> 29)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    let len = KIB + usize::try_from((mix >> 8) % (2 * MIB as u64 - KIB as u64)).unwrap();
    frame(1 + index % 4, index, index % 9 == 0, len)
}

// ---- a node that sends and a node that receives -------------------------------------------------

/// Wait for the next event that is a media frame; other events fail the test.
async fn next_media(node: &mut Node) -> Arc<[u8]> {
    match node.next().await {
        LinkEvent::Media { data, .. } => data,
        other => panic!("expected a media frame, got {other:?}"),
    }
}

/// Send `frame`, retrying while the transport is congested.
async fn send_when_possible(sender: &Transport, peer: NodeId, frame: Arc<[u8]>) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match sender.send_media(peer, frame.clone()) {
            Ok(()) => return,
            Err(LinkError::Congested) => {
                assert!(Instant::now() < deadline, "congested for 30 s");
                sleep(Duration::from_millis(2)).await;
            }
            Err(other) => panic!("send_media failed: {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_20_mib_frame_round_trips_byte_exact() {
    let (a, mut b) = connected_pair().await;
    let sent = frame(3, 1, true, 20 * MIB);
    a.transport.send_media(b.id, sent.clone()).unwrap();
    let received = next_media(&mut b).await;
    assert_eq!(received.len(), sent.len());
    assert!(*received == *sent, "the frame arrived changed");
    let header = read_header(&received).unwrap();
    assert_eq!((header.projection, header.key), (3, true));
    // The sender gets nothing back, and nothing else arrives.
    b.expect_quiet(Duration::from_millis(200)).await;
}

/// Not a pass/fail measure of speed: reports what loopback does with 20 MiB frames.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loopback_throughput_of_20_mib_frames() {
    let (a, mut b) = connected_pair().await;
    // The first frame warms the connection up (congestion window, buffers).
    a.transport.send_media(b.id, frame(1, 0, true, 20 * MIB)).unwrap();
    next_media(&mut b).await;

    const FRAMES: u64 = 10;
    let mut elapsed = Duration::ZERO;
    for seq in 1..=FRAMES {
        let sent = frame(1, seq, true, 20 * MIB);
        let started = Instant::now();
        send_when_possible(&a.transport, b.id, sent.clone()).await;
        let received = next_media(&mut b).await;
        elapsed += started.elapsed();
        assert!(*received == *sent);
    }
    let megabytes = (FRAMES as f64) * (20 * MIB) as f64 / 1e6;
    eprintln!(
        "media throughput: {:.0} MB/s ({FRAMES} frames of 20 MiB, one at a time, {:.2} s)",
        megabytes / elapsed.as_secs_f64(),
        elapsed.as_secs_f64()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_hundred_frames_of_random_sizes_all_arrive_intact() {
    const FRAMES: u64 = 200;
    let (a, mut b) = connected_pair().await;
    let peer = b.id;
    let sender = tokio::spawn(async move {
        for index in 0..FRAMES {
            send_when_possible(&a.transport, peer, random_frame(index)).await;
        }
        a
    });

    // Frames may arrive in any order: each one says which it is (its `seq`), and is compared with
    // a rebuilt copy.
    let mut arrived = HashSet::new();
    let mut bytes = 0usize;
    for _ in 0..FRAMES {
        let received = next_media(&mut b).await;
        let index = read_header(&received).unwrap().seq;
        assert!(index < FRAMES, "a frame nobody sent: {index}");
        assert!(arrived.insert(index), "frame {index} arrived twice");
        assert!(*received == *random_frame(index), "frame {index} is damaged");
        bytes += received.len();
    }
    assert_eq!(arrived.len(), FRAMES as usize);
    eprintln!("{FRAMES} frames, {} MiB, all intact", bytes / MIB);
    let a = sender.await.unwrap();
    b.expect_quiet(Duration::from_millis(200)).await;
    drop(a);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_real_cpf1_key_frame_decodes_to_the_pixels_that_were_sent() {
    let (a, mut b) = connected_pair().await;
    let (width, height) = (1280u32, 720u32);
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    for (index, word) in pixels.chunks_mut(4).enumerate() {
        let value = (index as u32).wrapping_mul(2_654_435_761);
        word.copy_from_slice(&value.to_le_bytes());
    }
    let mut encoder = TileEncoder::new();
    let mut out = Vec::new();
    let header = FrameHeader {
        projection: 42,
        seq: 1,
        key: true,
        captured_ns: 7,
        width,
        height,
    };
    encoder
        .encode(header, &pixels, width * 4, false, &mut out)
        .unwrap();
    a.transport.send_media(b.id, Arc::from(out)).unwrap();

    let received = next_media(&mut b).await;
    let mut decoder = TileDecoder::new();
    let (applied, _) = decoder.apply(&received).unwrap();
    assert_eq!((applied.projection, applied.seq, applied.key), (42, 1, true));
    assert!(decoder.canvas().0 == pixels.as_slice(), "the canvas differs");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn media_can_flow_both_ways_at_once() {
    let (mut a, mut b) = connected_pair().await;
    let (from_a, from_b) = (frame(1, 1, true, 5 * MIB), frame(2, 2, true, 7 * MIB));
    a.transport.send_media(b.id, from_a.clone()).unwrap();
    b.transport.send_media(a.id, from_b.clone()).unwrap();
    assert!(*next_media(&mut b).await == *from_a);
    assert!(*next_media(&mut a).await == *from_b);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn media_does_not_count_toward_the_input_rate_limit() {
    // 2,500 media frames and 1,500 key presses, all within about a second: the key presses alone
    // fit under the limit (2,000 a second), so every one must arrive. The media is not dropped
    // or throttled either.
    const FRAMES: u32 = 2_500;
    const KEYS: u32 = 1_500;
    let (a, mut b) = connected_pair().await;
    let peer = b.id;
    let mut link = a.transport.link(peer).unwrap();
    let transport = a.transport.clone();
    let flood = tokio::spawn(async move {
        for index in 0..FRAMES {
            // Spread over 100 projections: three unfinished frames each is plenty of room.
            let frame = delta(u64::from(index % 100), 200);
            loop {
                match transport.send_media(peer, frame.clone()) {
                    Ok(()) => break,
                    Err(LinkError::Congested) => sleep(Duration::from_millis(1)).await,
                    Err(other) => panic!("send_media failed: {other:?}"),
                }
            }
        }
    });
    for batch in 0..15u32 {
        for i in 0..100 {
            link.send_input(&key_down(batch * 100 + i + 1)).unwrap();
        }
        sleep(Duration::from_millis(50)).await;
    }
    flood.await.unwrap();

    let (mut frames, mut keys) = (0, 0);
    while frames < FRAMES || keys < KEYS {
        match b.next().await {
            LinkEvent::Media { .. } => frames += 1,
            LinkEvent::Input { .. } => keys += 1,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!((frames, keys), (FRAMES, KEYS));
    b.expect_quiet(Duration::from_millis(200)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn control_and_input_arrive_promptly_while_a_30_mib_frame_is_in_flight() {
    let (a, mut b) = connected_pair().await;
    let mut link = a.transport.link(b.id).unwrap();
    let sent = frame(9, 1, true, 30 * MIB);
    a.transport.send_media(b.id, sent.clone()).unwrap();

    let mut worst = Duration::ZERO;
    let mut rounds_during_media = 0;
    let mut media = None;
    // Keep sending a ping and a key press until the frame is through, and three rounds after.
    let (mut round, mut after) = (0u64, 0);
    while media.is_none() || after < 3 {
        round += 1;
        let started = Instant::now();
        link.send_control(&ControlMessage::Ping { t0: round }).unwrap();
        link.send_input(&key_down(u32::try_from(round).unwrap())).unwrap();
        let (mut ping, mut input) = (false, false);
        while !(ping && input) {
            match b.next().await {
                LinkEvent::Control {
                    msg: ControlMessage::Ping { t0 },
                    ..
                } => {
                    assert_eq!(t0, round);
                    ping = true;
                }
                LinkEvent::Input { .. } => input = true,
                LinkEvent::Media { data, .. } => media = Some(data),
                other => panic!("unexpected {other:?}"),
            }
        }
        let took = started.elapsed();
        worst = worst.max(took);
        assert!(
            took < Duration::from_millis(100),
            "round {round} took {took:?} with a 30 MiB frame in flight"
        );
        if media.is_none() {
            rounds_during_media += 1;
        } else {
            after += 1;
        }
        sleep(Duration::from_millis(5)).await;
    }
    eprintln!("{rounds_during_media} control+input rounds during the transfer, worst {worst:?}");
    assert!(
        rounds_during_media >= 1,
        "the frame was through before the first ping: the test did not overlap them"
    );
    assert!(*media.unwrap() == *sent);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sending_to_an_unknown_or_departed_peer_is_closed() {
    let (a, b) = pair();
    let stranger = NodeId([7; 32]);
    assert_eq!(
        a.transport.send_media(stranger, delta(1, 1_000)),
        Err(LinkError::Closed)
    );
    // Known but not connected.
    assert_eq!(
        a.transport.send_media(b.id, delta(1, 1_000)),
        Err(LinkError::Closed)
    );

    let (mut a, b) = connected_pair().await;
    a.transport.send_media(b.id, delta(1, 1_000)).unwrap();
    let peer = b.id;
    b.transport.shutdown("bye").await;
    a.expect_closed(peer, LinkError::Closed).await;
    // The delta may have been delivered, which is fine; what matters is the send after the close.
    assert_eq!(
        a.transport.send_media(peer, delta(1, 1_000)),
        Err(LinkError::Closed)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_closing_link_refuses_media() {
    let (a, b) = connected_pair().await;
    a.transport.link(b.id).unwrap().close("done");
    assert_eq!(
        a.transport.send_media(b.id, delta(1, 1_000)),
        Err(LinkError::Closed)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn frames_over_64_mib_are_refused_at_the_sender_and_exactly_64_mib_is_carried() {
    let (a, mut b) = connected_pair().await;
    assert_eq!(
        a.transport.send_media(b.id, delta(1, 64 * MIB + 1)),
        Err(LinkError::Invalid("media frame too large"))
    );
    // Refused for good, not "try again later": and the connection is none the worse.
    let exact = key_frame(1, 64 * MIB);
    a.transport.send_media(b.id, exact.clone()).unwrap();
    // Nothing else fits while those bytes are unfinished.
    assert_eq!(
        a.transport.send_media(b.id, key_frame(2, HEADER)),
        Err(LinkError::Congested)
    );
    let received = next_media(&mut b).await;
    assert!(*received == *exact);
}

// ---- a raw peer that reads when it is told to --------------------------------------------------

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
        match timeout(WAIT, self.node.events.recv())
            .await
            .expect("timed out waiting for the protocol error")
            .expect("the event sink was dropped")
        {
            LinkEvent::Closed {
                peer,
                error: LinkError::Invalid(why),
            } => {
                assert_eq!(peer, self.peer);
                eprintln!("protocol error reported: {why}");
            }
            other => panic!("expected Closed with Invalid, got {other:?}"),
        }
        let (code, reason) = closed_by_peer(&self.conn).await;
        assert_eq!((code, reason.as_str()), (1, "protocol error"));
        self.node.expect_quiet(Duration::from_millis(400)).await;
        assert!(self.node.transport.peers().is_empty());
    }
}

/// Accept the node's streams until `media` of them are media streams, reading each to its end in
/// the background. The node's control and input streams are kept open and unread.
struct Drain {
    keep: Vec<quinn::RecvStream>,
    readers: tokio::task::JoinSet<usize>,
}

impl Drain {
    fn new() -> Drain {
        Drain {
            keep: Vec::new(),
            readers: tokio::task::JoinSet::new(),
        }
    }

    /// Read `media` media streams to their end; returns the lengths (type byte excluded).
    async fn read(&mut self, conn: &quinn::Connection, media: usize) -> Vec<usize> {
        let mut lengths = Vec::new();
        let mut started = 0;
        while lengths.len() < media {
            tokio::select! {
                accepted = conn.accept_uni(), if started < media => {
                    let mut recv = accepted.unwrap();
                    let mut kind = [0u8; 1];
                    recv.read_exact(&mut kind).await.unwrap();
                    if kind[0] == 0x03 {
                        started += 1;
                        self.readers.spawn(async move { recv.read_to_end(usize::MAX).await.unwrap().len() });
                    } else {
                        self.keep.push(recv);
                    }
                }
                done = self.readers.join_next(), if !self.readers.is_empty() => {
                    lengths.push(done.unwrap().unwrap());
                }
            }
        }
        lengths
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_that_does_not_read_gets_backpressure_not_a_queue() {
    let mut rig = rig().await;
    let _control = rig.say_hello().await;
    let node = &rig.node.transport;
    let peer = rig.peer;
    // Frames larger than the raw peer's flow-control window (1.25 MB) cannot finish while it does
    // not read, so they stay unfinished.
    let projection = 5;
    for _ in 0..3 {
        node.send_media(peer, delta(projection, 4 * MIB)).unwrap();
    }
    // The 4th unfinished delta frame of a projection is refused...
    assert_eq!(
        node.send_media(peer, delta(projection, 4 * MIB)),
        Err(LinkError::Congested)
    );
    // ...and so is one far smaller: it is the count, not the size.
    assert_eq!(
        node.send_media(peer, delta(projection, HEADER)),
        Err(LinkError::Congested)
    );
    // Another projection has its own count.
    node.send_media(peer, delta(6, 4 * MIB)).unwrap();
    // A key frame goes past the count...
    node.send_media(peer, key_frame(projection, 4 * MIB)).unwrap();
    // ...and after it a delta is refused still.
    assert_eq!(
        node.send_media(peer, delta(projection, HEADER)),
        Err(LinkError::Congested)
    );
    // 5 x 4 MiB are unfinished now, 4 on this projection. Fill the 64 MiB with key frames (they
    // ignore the count): 44 MiB more.
    for _ in 0..2 {
        node.send_media(peer, key_frame(projection, 20 * MIB)).unwrap();
    }
    node.send_media(peer, key_frame(projection, 4 * MIB)).unwrap();
    // 64 MiB exactly. Now nothing fits, key frame or not.
    assert_eq!(
        node.send_media(peer, key_frame(projection, HEADER)),
        Err(LinkError::Congested)
    );
    assert_eq!(
        node.send_media(peer, delta(77, HEADER)),
        Err(LinkError::Congested)
    );
    // The refusals did not cost the connection anything.
    assert_eq!(node.peers(), vec![peer]);

    // Let the peer read: the frames finish, their bytes return, sending works again.
    let mut drain = Drain::new();
    let lengths = drain.read(&rig.conn, 8).await;
    assert_eq!(lengths.iter().sum::<usize>(), 64 * MIB);
    // A projection of its own, so the counts below do not depend on how fast the old frames are
    // acknowledged: the first send waits for room, the rest are tiny.
    send_when_possible(node, peer, delta(90, 4 * MIB)).await;
    node.send_media(peer, delta(90, HEADER)).unwrap();
    node.send_media(peer, delta(90, HEADER)).unwrap();
    assert_eq!(
        node.send_media(peer, delta(90, HEADER)),
        Err(LinkError::Congested)
    );
    // The projection that was full works again too.
    send_when_possible(node, peer, delta(projection, HEADER)).await;
    let mut lengths = drain.read(&rig.conn, 4).await;
    lengths.sort_unstable();
    assert_eq!(lengths, vec![HEADER, HEADER, HEADER, 4 * MIB]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unfinished_frames_are_dropped_silently_when_the_connection_closes() {
    let mut rig = rig().await;
    let _control = rig.say_hello().await;
    for projection in 1..=3 {
        rig.node
            .transport
            .send_media(rig.peer, delta(projection, 8 * MIB))
            .unwrap();
    }
    sleep(Duration::from_millis(100)).await;
    rig.conn.close(0u32.into(), b"gone");
    // The engine hears one Closed, as for any link, and nothing about the frames.
    rig.node.expect_closed(rig.peer, LinkError::Closed).await;
    rig.node.expect_quiet(Duration::from_millis(400)).await;
    assert!(rig.node.transport.peers().is_empty());
    assert_eq!(
        rig.node.transport.send_media(rig.peer, delta(1, HEADER)),
        Err(LinkError::Closed)
    );
}

// ---- receiving from a peer that misbehaves -----------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_65_mib_stream_closes_the_connection_with_a_protocol_error() {
    let mut rig = rig().await;
    let _control = rig.say_hello().await;
    let conn = rig.conn.clone();
    // The writer stops when the node closes the connection.
    let writer = tokio::spawn(async move {
        let mut send = conn.open_uni().await.unwrap();
        send.write_all(&[0x03]).await.unwrap();
        let chunk = vec![0xab; MIB];
        for _ in 0..65 {
            if send.write_all(&chunk).await.is_err() {
                return;
            }
        }
        let _ = send.finish();
        // Keep the stream alive until the connection is gone.
        let _ = send.stopped().await;
    });
    rig.expect_protocol_error().await;
    writer.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_raw_peer_can_send_a_frame_of_exactly_64_mib() {
    let mut rig = rig().await;
    let _control = rig.say_hello().await;
    let mut send = rig.conn.open_uni().await.unwrap();
    send.write_all(&[0x03]).await.unwrap();
    let chunk = vec![0xcd; MIB];
    for _ in 0..64 {
        send.write_all(&chunk).await.unwrap();
    }
    send.finish().unwrap();
    let received = next_media(&mut rig.node).await;
    assert_eq!(received.len(), 64 * MIB);
    assert!(received.iter().all(|byte| *byte == 0xcd));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn media_before_hello_closes_the_connection() {
    let mut rig = rig().await;
    let _media = open_stream(&rig.conn, 0x03, &delta(1, 1_000)).await;
    rig.expect_protocol_error().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reset_media_stream_is_dropped_without_harm() {
    let mut rig = rig().await;
    let mut control = rig.say_hello().await;
    let mut stale = open_stream(&rig.conn, 0x03, &delta(1, 100_000)).await;
    stale.reset(7u32.into()).unwrap();
    // A frame after it still arrives, and so does control traffic: the connection is fine.
    let good = key_frame(1, 5_000);
    let mut fresh = open_stream(&rig.conn, 0x03, &good).await;
    fresh.finish().unwrap();
    assert!(*next_media(&mut rig.node).await == *good);
    let mut ping = Vec::new();
    crosspane_protocol::wire::encode_control(&ControlMessage::Ping { t0: 5 }, &mut ping).unwrap();
    control.write_all(&ping).await.unwrap();
    assert!(matches!(
        rig.node.next().await,
        LinkEvent::Control {
            msg: ControlMessage::Ping { t0: 5 },
            ..
        }
    ));
    rig.node.expect_quiet(Duration::from_millis(300)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_frame_cut_off_by_the_connection_closing_is_never_delivered() {
    let mut rig = rig().await;
    let _control = rig.say_hello().await;
    // Half a frame, no FIN, then the peer goes away.
    let _half = open_stream(&rig.conn, 0x03, &delta(1, 500_000)).await;
    sleep(Duration::from_millis(100)).await;
    rig.conn.close(0u32.into(), b"gone");
    rig.node.expect_closed(rig.peer, LinkError::Closed).await;
    rig.node.expect_quiet(Duration::from_millis(300)).await;
}
