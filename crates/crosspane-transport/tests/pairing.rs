//! Pairing connections over real UDP on loopback (WP-1.38).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use common::{Node, Raw, WAIT, identity, loopback};
use crosspane_protocol::msg::Capability;
use crosspane_protocol::wire::{KIND_CONTROL, KIND_PAIRING, WIRE_VERSION};
use crosspane_security::identity::DeviceIdentity;
use crosspane_security::pairing::{
    Event, Initiator, Joiner, Local, PairingError, PairingMsg, Peer, Sas,
};
use crosspane_security::rng::SystemRng;
use crosspane_transport::TransportError;
use crosspane_transport::pairing::{PAIRING_ALPN, PairingChannel, PairingListener, pair_connect};
use quinn::{ConnectionError, VarInt};
use tokio::sync::oneshot;
use tokio::time::timeout;

/// A frame as the pairing stream carries it.
fn frame(version: u8, kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![version, kind, 0, 0];
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

fn pairing_frame(payload: &[u8]) -> Vec<u8> {
    frame(WIRE_VERSION, KIND_PAIRING, payload)
}

/// A listener, and one connection to it from each side.
struct Connected {
    listener: PairingListener,
    listener_id: Arc<DeviceIdentity>,
    dialer_id: Arc<DeviceIdentity>,
    /// The listener's end.
    accepted: PairingChannel,
    /// The dialer's end.
    dialed: PairingChannel,
}

async fn dial_and_accept(
    listener: &PairingListener,
    dialer_id: &Arc<DeviceIdentity>,
) -> (PairingChannel, PairingChannel) {
    let (accepted, dialed) = timeout(WAIT, async {
        tokio::join!(
            listener.accept(),
            pair_connect(listener.local_addr(), dialer_id.clone())
        )
    })
    .await
    .expect("pairing connection timed out");
    (accepted.unwrap(), dialed.unwrap())
}

async fn connected() -> Connected {
    let listener_id = identity();
    let dialer_id = identity();
    let listener = PairingListener::bind(loopback(), listener_id.clone()).unwrap();
    let (accepted, dialed) = dial_and_accept(&listener, &dialer_id).await;
    Connected {
        listener,
        listener_id,
        dialer_id,
        accepted,
        dialed,
    }
}

// ---- 1. identities and the exporter -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_ends_see_the_other_key_and_share_an_exporter_that_is_unique_per_connection() {
    let first = connected().await;
    assert_eq!(first.accepted.peer_spki(), first.dialer_id.spki());
    assert_eq!(first.dialed.peer_spki(), first.listener_id.spki());
    assert_eq!(first.accepted.peer_spki().len(), 91);

    let exporter = first.accepted.exporter();
    assert_eq!(exporter, first.dialed.exporter());
    assert_ne!(exporter, [0; 32], "the exporter is not all zero");

    // A second connection to the same listener, from the same dialer: a different exporter, but
    // again equal on both ends.
    let (accepted, dialed) = dial_and_accept(&first.listener, &first.dialer_id).await;
    assert_eq!(accepted.exporter(), dialed.exporter());
    assert_ne!(accepted.exporter(), exporter);
    assert_ne!(dialed.exporter(), first.dialed.exporter());
    assert_eq!(accepted.peer_spki(), first.dialer_id.spki());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn messages_cross_in_both_directions_even_when_they_arrive_in_pieces() {
    let mut c = connected().await;
    c.accepted.send(&PairingMsg::Commit([7; 32])).await.unwrap();
    assert_eq!(c.dialed.recv().await.unwrap(), PairingMsg::Commit([7; 32]));
    let reveal = PairingMsg::Reveal {
        nonce: [9; 32],
        spki: c.dialer_id.spki().to_vec(),
    };
    c.dialed.send(&reveal).await.unwrap();
    assert_eq!(c.accepted.recv().await.unwrap(), reveal);

    // A raw peer writing a frame one byte at a time: the receiver reassembles it.
    let server = identity();
    let listener = PairingListener::bind(loopback(), server.clone()).unwrap();
    let (_raw, _conn, mut send, mut channel) = raw_pairing_peer(&listener, &server).await;
    for byte in pairing_frame(&PairingMsg::Matched.encode()) {
        send.write_all(&[byte]).await.unwrap();
        tokio::task::yield_now().await;
    }
    assert_eq!(channel.recv().await.unwrap(), PairingMsg::Matched);
}

// ---- 2 and 3. a full exchange -------------------------------------------------------------------

#[derive(Debug)]
enum Outcome {
    Paired(Peer),
    Failed(PairingError),
}

fn local(identity: &DeviceIdentity, name: &str) -> Local {
    Local {
        spki: identity.spki().to_vec(),
        name: name.to_owned(),
        grants: vec![Capability::InputAccept, Capability::WindowShare],
    }
}

/// Run the listener side as the `Initiator`.
async fn drive_initiator(
    mut channel: PairingChannel,
    local: Local,
    sas_out: oneshot::Sender<Sas>,
    confirm: bool,
) -> Outcome {
    let (mut machine, events) = Initiator::start(local, channel.exporter(), &mut SystemRng);
    let mut queue: VecDeque<Event> = events.into();
    let mut sas_out = Some(sas_out);
    loop {
        while let Some(event) = queue.pop_front() {
            match event {
                Event::Send(msg) => channel.send(&msg).await.unwrap(),
                Event::ShowSas(sas) => sas_out.take().unwrap().send(sas).unwrap(),
                Event::AskConfirm => queue.extend(machine.user_confirmed(confirm)),
                Event::Paired(peer) => {
                    channel.close("paired").await;
                    return Outcome::Paired(peer);
                }
                Event::Failed(error) => {
                    channel.close("failed").await;
                    return Outcome::Failed(error);
                }
                Event::ShowCandidates(_) => panic!("the initiator shows a code, not candidates"),
            }
        }
        let msg = channel.recv().await.unwrap();
        queue.extend(machine.on_message(msg));
    }
}

/// Run the dialer side as the `Joiner`; `pick_right` picks the initiator's code.
async fn drive_joiner(
    mut channel: PairingChannel,
    local: Local,
    sas_in: oneshot::Receiver<Sas>,
    pick_right: bool,
) -> Outcome {
    let (mut machine, events) = Joiner::start(local, channel.exporter(), &mut SystemRng);
    let mut queue: VecDeque<Event> = events.into();
    let mut sas_in = Some(sas_in);
    loop {
        while let Some(event) = queue.pop_front() {
            match event {
                Event::Send(msg) => channel.send(&msg).await.unwrap(),
                Event::ShowCandidates(candidates) => {
                    let sas = sas_in.take().unwrap().await.unwrap();
                    assert!(candidates.contains(&sas), "the right code is on offer");
                    let pick = if pick_right {
                        sas
                    } else {
                        candidates.into_iter().find(|c| *c != sas).unwrap()
                    };
                    queue.extend(machine.user_picked(pick));
                }
                Event::Paired(peer) => {
                    channel.close("paired").await;
                    return Outcome::Paired(peer);
                }
                Event::Failed(error) => {
                    channel.close("failed").await;
                    return Outcome::Failed(error);
                }
                Event::ShowSas(_) | Event::AskConfirm => {
                    panic!("the joiner picks a code, it does not show or confirm one")
                }
            }
        }
        let msg = channel.recv().await.unwrap();
        queue.extend(machine.on_message(msg));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_exchange_pairs_both_sides_with_the_keys_tls_presented() {
    let c = connected().await;
    let initiator_sees = c.accepted.peer_spki().to_vec();
    let joiner_sees = c.dialed.peer_spki().to_vec();
    let (sas_tx, sas_rx) = oneshot::channel();
    let initiator = tokio::spawn(drive_initiator(
        c.accepted,
        local(&c.listener_id, "initiator"),
        sas_tx,
        true,
    ));
    let joiner = tokio::spawn(drive_joiner(
        c.dialed,
        local(&c.dialer_id, "joiner"),
        sas_rx,
        true,
    ));
    let (initiator, joiner) = timeout(WAIT, async { tokio::join!(initiator, joiner) })
        .await
        .expect("the exchange timed out");

    let Outcome::Paired(joiner_as_seen_by_initiator) = initiator.unwrap() else {
        panic!("the initiator did not pair");
    };
    let Outcome::Paired(initiator_as_seen_by_joiner) = joiner.unwrap() else {
        panic!("the joiner did not pair");
    };
    // Each side pins the other's key, and it is the key TLS proved possession of.
    assert_eq!(joiner_as_seen_by_initiator.spki, c.dialer_id.spki());
    assert_eq!(joiner_as_seen_by_initiator.spki, initiator_sees);
    assert_eq!(joiner_as_seen_by_initiator.node, c.dialer_id.node());
    assert_eq!(joiner_as_seen_by_initiator.name, "joiner");
    assert_eq!(initiator_as_seen_by_joiner.spki, c.listener_id.spki());
    assert_eq!(initiator_as_seen_by_joiner.spki, joiner_sees);
    assert_eq!(initiator_as_seen_by_joiner.node, c.listener_id.node());
    assert_eq!(initiator_as_seen_by_joiner.name, "initiator");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrong_pick_fails_both_sides_and_pairs_nothing() {
    let c = connected().await;
    let (sas_tx, sas_rx) = oneshot::channel();
    let initiator = tokio::spawn(drive_initiator(
        c.accepted,
        local(&c.listener_id, "initiator"),
        sas_tx,
        true,
    ));
    let joiner = tokio::spawn(drive_joiner(
        c.dialed,
        local(&c.dialer_id, "joiner"),
        sas_rx,
        false,
    ));
    let (initiator, joiner) = timeout(WAIT, async { tokio::join!(initiator, joiner) })
        .await
        .expect("the exchange timed out");

    let Outcome::Failed(joiner_error) = joiner.unwrap() else {
        panic!("the joiner paired after a wrong pick");
    };
    assert_eq!(joiner_error, PairingError::CodeMismatch);
    let Outcome::Failed(initiator_error) = initiator.unwrap() else {
        panic!("the initiator paired after the joiner's wrong pick");
    };
    assert!(matches!(initiator_error, PairingError::PeerAborted(_)));
}

// ---- 4. protocol faults -------------------------------------------------------------------------

/// A raw client that opens the pairing stream properly, and the listener's channel for it.
async fn raw_pairing_peer(
    listener: &PairingListener,
    server: &DeviceIdentity,
) -> (Raw, quinn::Connection, quinn::SendStream, PairingChannel) {
    let raw = Raw::new();
    let conn = raw
        .connect_with_alpn(&identity(), server, listener.local_addr(), PAIRING_ALPN)
        .await
        .unwrap();
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    send.write_all(&pairing_frame(&[])).await.unwrap();
    let channel = timeout(WAIT, listener.accept()).await.unwrap().unwrap();
    (raw, conn, send, channel)
}

/// The connection was closed with application code 1.
async fn assert_closed_with_protocol_error(conn: &quinn::Connection) {
    match timeout(WAIT, conn.closed()).await.expect("never closed") {
        ConnectionError::ApplicationClosed(close) => {
            assert_eq!(close.error_code, VarInt::from_u32(1));
        }
        other => panic!("expected an application close with code 1, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_frames_close_the_connection_and_fail_recv() {
    let cases: [(&str, Vec<u8>); 5] = [
        (
            "garbage payload",
            pairing_frame(&[0xee, 1, 2, 3, 4, 5, 6, 7]),
        ),
        // 1025 bytes: one over the cap.
        ("oversized payload", pairing_frame(&[0; 1025])),
        (
            "a header that only declares an oversized payload",
            // 4096 bytes, little endian, and no payload at all.
            vec![WIRE_VERSION, KIND_PAIRING, 0, 0, 0x00, 0x10, 0, 0],
        ),
        (
            "another kind",
            frame(WIRE_VERSION, KIND_CONTROL, &PairingMsg::Matched.encode()),
        ),
        ("an unsupported wire version", frame(9, KIND_PAIRING, &[3])),
    ];
    for (name, bytes) in cases {
        let server = identity();
        let listener = PairingListener::bind(loopback(), server.clone()).unwrap();
        let (_raw, conn, mut send, mut channel) = raw_pairing_peer(&listener, &server).await;
        // A valid message first: all is well until the fault.
        send.write_all(&pairing_frame(&PairingMsg::Matched.encode()))
            .await
            .unwrap();
        assert_eq!(channel.recv().await.unwrap(), PairingMsg::Matched, "{name}");

        send.write_all(&bytes).await.unwrap();
        let error = timeout(WAIT, channel.recv())
            .await
            .unwrap_or_else(|_| panic!("{name}: recv hung"))
            .expect_err(name);
        assert!(
            matches!(&error, TransportError::Connect(why) if why.contains("protocol error")),
            "{name}: {error:?}"
        );
        assert_closed_with_protocol_error(&conn).await;
        // The channel stays failed.
        assert!(channel.recv().await.is_err(), "{name}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_frame_after_the_stream_marker_is_a_protocol_error() {
    let server = identity();
    let listener = PairingListener::bind(loopback(), server.clone()).unwrap();
    let (_raw, conn, mut send, mut channel) = raw_pairing_peer(&listener, &server).await;
    // The marker is only the first frame.
    send.write_all(&pairing_frame(&[])).await.unwrap();
    assert!(channel.recv().await.is_err());
    assert_closed_with_protocol_error(&conn).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_that_does_not_open_with_the_marker_is_never_handed_out() {
    let server = identity();
    let listener = PairingListener::bind(loopback(), server.clone()).unwrap();
    let raw = Raw::new();
    let conn = raw
        .connect_with_alpn(&identity(), &server, listener.local_addr(), PAIRING_ALPN)
        .await
        .unwrap();
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    // A real message where the marker belongs.
    send.write_all(&pairing_frame(&PairingMsg::Matched.encode()))
        .await
        .unwrap();
    assert_closed_with_protocol_error(&conn).await;
    assert!(
        timeout(Duration::from_millis(300), listener.accept())
            .await
            .is_err(),
        "nothing to accept"
    );
}

// ---- 5. the two transports are separate ---------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_normal_transport_refuses_a_pairing_dial() {
    let dialer = identity();
    // The dialer is even pinned, so only the ALPN can keep it out.
    let mut node = Node::start("node", identity(), &[&dialer]);
    let error = pair_connect(node.addr(), dialer).await.unwrap_err();
    assert!(
        matches!(&error, TransportError::Connect(why) if why.contains("does not accept pairing")),
        "{error:?}"
    );
    // No link, no event.
    assert!(node.transport.peers().is_empty());
    node.expect_quiet(Duration::from_millis(300)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_pairing_listener_refuses_a_normal_connect() {
    let listener_id = identity();
    let listener = PairingListener::bind(loopback(), listener_id.clone()).unwrap();
    // The node even pins the listener's key, so only the ALPN can keep it out.
    let mut node = Node::start("node", identity(), &[&listener_id]);
    let error = node.transport.connect(listener.local_addr()).await;
    assert!(
        matches!(error, Err(TransportError::Connect(_))),
        "{error:?}"
    );
    assert!(node.transport.peers().is_empty());
    node.expect_quiet(Duration::from_millis(300)).await;
    assert!(
        timeout(Duration::from_millis(300), listener.accept())
            .await
            .is_err(),
        "nothing to accept"
    );
}

// ---- 6. lifetime --------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_the_listener_stops_new_dials() {
    let listener = PairingListener::bind(loopback(), identity()).unwrap();
    let addr = listener.local_addr();
    // Works while it exists.
    let dialer = identity();
    let (_accepted, _dialed) = dial_and_accept(&listener, &dialer).await;
    drop(listener);
    assert!(pair_connect(addr, dialer).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_channel_that_was_accepted_survives_dropping_the_listener() {
    let Connected {
        listener,
        mut accepted,
        mut dialed,
        ..
    } = connected().await;
    drop(listener);
    dialed.send(&PairingMsg::Matched).await.unwrap();
    assert_eq!(accepted.recv().await.unwrap(), PairingMsg::Matched);
    accepted.send(&PairingMsg::Commit([1; 32])).await.unwrap();
    assert_eq!(dialed.recv().await.unwrap(), PairingMsg::Commit([1; 32]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_delivers_what_was_sent_first() {
    let Connected {
        mut accepted,
        mut dialed,
        ..
    } = connected().await;
    dialed.send(&PairingMsg::Matched).await.unwrap();
    dialed.close("done").await;
    assert_eq!(accepted.recv().await.unwrap(), PairingMsg::Matched);
    assert!(accepted.recv().await.is_err(), "then the close is seen");
}

// ---- limits -------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn at_most_four_handshakes_run_at_once_and_slots_come_back() {
    let server = identity();
    let listener = PairingListener::bind(loopback(), server.clone()).unwrap();
    let addr = listener.local_addr();
    let raw = Raw::new();
    let client = identity();

    // Four peers finish TLS and then stall without opening a stream: each holds a handshake slot.
    let mut holders = Vec::new();
    for _ in 0..4 {
        holders.push(
            raw.connect_with_alpn(&client, &server, addr, PAIRING_ALPN)
                .await
                .unwrap(),
        );
    }
    // The fifth is refused, whoever it is.
    assert!(
        raw.connect_with_alpn(&client, &server, addr, PAIRING_ALPN)
            .await
            .is_err()
    );
    assert!(pair_connect(addr, identity()).await.is_err());

    // The slots return when the stalled peers go away.
    drop(holders);
    let dialer = identity();
    let mut paired = None;
    for _ in 0..50 {
        if let Ok(channel) = pair_connect(addr, dialer.clone()).await {
            paired = Some(channel);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _dialed = paired.expect("handshake slots were never released");
    let accepted = timeout(WAIT, listener.accept()).await.unwrap().unwrap();
    assert_eq!(accepted.peer_spki(), dialer.spki());
}
