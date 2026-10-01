//! E2 media frames: one unidirectional stream per frame, type byte `0x03` (E2 v0, decision 3).
//!
//! # Sending
//!
//! [`Transport::send_media`](crate::Transport::send_media) never blocks. It reserves the frame in
//! the connection's [`SendBudget`] and hands it to a task that opens a stream, writes the type byte
//! and the frame, finishes the stream, and keeps the reservation until the peer has acknowledged
//! every byte. So "unfinished" means "not yet fully received by the peer", and memory held for
//! media is bounded by [`MAX_UNFINISHED_BYTES`] per peer: a peer that stops reading makes
//! `send_media` answer `Congested` instead of growing a queue.
//!
//! The frame header is the CPF1 header of `crosspane-media`. This crate reads just two things from
//! it, the projection id and the key-frame flag, and has no dependency on that crate.
//!
//! # Receiving
//!
//! Each accepted media stream is read to its end by a task of its own, so a large frame never
//! delays control or input. The session task delivers the finished frame. A stream longer than
//! [`MAX_FRAME`] is a protocol error. A stream that is reset, or still unfinished when the
//! connection closes, is dropped silently.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crosspane_protocol::link::LinkError;
use quinn::{Connection, RecvStream};

use crate::hub::lock;

/// First byte of a unidirectional stream: one media frame.
pub(crate) const STREAM_MEDIA: u8 = 0x03;
/// Media is sent after input (100) and control (50).
pub(crate) const PRIORITY_MEDIA: i32 = 10;
/// The largest frame, which is also the largest a stream may carry (the CPF1 limit).
pub(crate) const MAX_FRAME: usize = 64 * 1024 * 1024;
/// Unfinished frames per projection beyond which a delta frame is refused.
const MAX_UNFINISHED_FRAMES: usize = 3;
/// Unfinished media bytes per peer beyond which every frame is refused.
const MAX_UNFINISHED_BYTES: usize = MAX_FRAME;
/// Bytes of partly received media a connection may hold. A peer that respects
/// [`MAX_UNFINISHED_BYTES`] stays far below this; it only bounds a misbehaving one.
const MAX_RECEIVING_BYTES: usize = 4 * MAX_FRAME;
/// Largest read from a stream at once.
const READ_CHUNK: usize = 1024 * 1024;

/// Where the CPF1 header keeps the flags byte (bit 0 is the key-frame flag) and the little-endian
/// `u64` projection id.
const FLAGS_AT: usize = 5;
const PROJECTION_AT: std::ops::Range<usize> = 8..16;
const KEY_FLAG: u8 = 1;

/// The projection and key-frame flag from a frame's CPF1 header. A frame too short to have them
/// can't be a valid frame (the receiver will refuse it), so it is accounted as a delta frame of
/// projection 0 rather than rejected here.
pub(crate) fn frame_info(frame: &[u8]) -> (u64, bool) {
    let key = frame
        .get(FLAGS_AT)
        .is_some_and(|flags| flags & KEY_FLAG != 0);
    let projection = frame
        .get(PROJECTION_AT)
        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
        .map_or(0, u64::from_le_bytes);
    (projection, key)
}

// ---- sending -----------------------------------------------------------------------------------

#[derive(Debug, Default)]
struct SendState {
    bytes: usize,
    /// Unfinished frames per projection.
    frames: HashMap<u64, usize>,
}

/// What a connection has promised to the peer's media streams and not yet seen acknowledged.
#[derive(Debug, Default)]
pub(crate) struct SendBudget {
    state: Mutex<SendState>,
}

impl SendBudget {
    /// Reserve room for a frame of `len` bytes: `Congested` if that would put the peer over
    /// [`MAX_UNFINISHED_BYTES`], or a delta frame over [`MAX_UNFINISHED_FRAMES`] for its
    /// projection. A key frame ignores the frame count, so one can always follow a congested
    /// delta once the bytes drain.
    pub(crate) fn reserve(
        self: &Arc<Self>,
        projection: u64,
        key: bool,
        len: usize,
    ) -> Result<Reservation, LinkError> {
        let mut state = lock(&self.state);
        if len > MAX_UNFINISHED_BYTES.saturating_sub(state.bytes) {
            return Err(LinkError::Congested);
        }
        let unfinished = state.frames.get(&projection).copied().unwrap_or(0);
        if !key && unfinished >= MAX_UNFINISHED_FRAMES {
            return Err(LinkError::Congested);
        }
        state.frames.insert(projection, unfinished + 1);
        state.bytes += len;
        Ok(Reservation {
            budget: self.clone(),
            projection,
            len,
        })
    }
}

/// One frame's share of the budget, returned when it is dropped.
#[derive(Debug)]
pub(crate) struct Reservation {
    budget: Arc<SendBudget>,
    projection: u64,
    len: usize,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut state = lock(&self.budget.state);
        state.bytes = state.bytes.saturating_sub(self.len);
        if let Some(unfinished) = state.frames.get_mut(&self.projection) {
            *unfinished = unfinished.saturating_sub(1);
            if *unfinished == 0 {
                state.frames.remove(&self.projection);
            }
        }
    }
}

/// Send one frame on a new stream and hold `reservation` until the peer has all of it.
///
/// Failures are silent: if the connection is gone the link's `Closed` tells the engine, and a
/// frame the peer refused (it stopped the stream) is just a lost frame, which a sender that tracks
/// acknowledgements recovers with a key frame.
pub(crate) async fn write_frame(conn: Connection, frame: Arc<[u8]>, reservation: Reservation) {
    let _reservation = reservation;
    let Ok(mut send) = conn.open_uni().await else {
        return;
    };
    if send.set_priority(PRIORITY_MEDIA).is_err()
        || send.write_all(&[STREAM_MEDIA]).await.is_err()
        || send.write_all(&frame).await.is_err()
        || send.finish().is_err()
    {
        return;
    }
    // Resolves when the peer acknowledged everything (or stopped the stream, or the connection
    // closed): the frame is no longer unfinished.
    let _ = send.stopped().await;
}

// ---- receiving ---------------------------------------------------------------------------------

/// How reading one media stream ended.
#[derive(Debug)]
pub(crate) enum Received {
    /// The whole frame.
    Frame(Arc<[u8]>),
    /// Reset, or cut off by the connection closing: nothing to deliver, nothing wrong.
    Dropped,
    /// The peer broke the protocol; the reason is for the engine's `Closed` event.
    Fault(&'static str),
}

/// Bytes a connection holds for streams still being received, released when dropped.
struct Buffered {
    total: Arc<AtomicUsize>,
    mine: usize,
}

impl Buffered {
    /// Count `len` more bytes. `false` if the connection now holds too many.
    fn add(&mut self, len: usize) -> bool {
        self.mine += len;
        self.total.fetch_add(len, Ordering::Relaxed) + len <= MAX_RECEIVING_BYTES
    }
}

impl Drop for Buffered {
    fn drop(&mut self) {
        self.total.fetch_sub(self.mine, Ordering::Relaxed);
    }
}

/// Read a media stream (its type byte already consumed) to its end. `buffered` counts, across the
/// connection, the bytes of frames still being received.
pub(crate) async fn read_frame(mut recv: RecvStream, buffered: Arc<AtomicUsize>) -> Received {
    let mut held = Buffered {
        total: buffered,
        mine: 0,
    };
    let mut data: Vec<u8> = Vec::new();
    loop {
        match recv.read_chunk(READ_CHUNK, true).await {
            Ok(Some(chunk)) => {
                let len = chunk.bytes.len();
                if len > MAX_FRAME - data.len() {
                    return Received::Fault("media frame too large");
                }
                if !held.add(len) {
                    return Received::Fault("too much unfinished media");
                }
                data.extend_from_slice(&chunk.bytes);
            }
            Ok(None) => return Received::Frame(Arc::from(data)),
            Err(_) => return Received::Dropped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget() -> Arc<SendBudget> {
        Arc::new(SendBudget::default())
    }

    #[test]
    fn the_projection_and_key_flag_come_from_the_cpf1_header() {
        let mut frame = vec![0u8; 48];
        frame[..4].copy_from_slice(&0x3146_5043u32.to_le_bytes());
        frame[4] = 1;
        frame[5] = 1;
        frame[8..16].copy_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(frame_info(&frame), (0x0102_0304_0506_0708, true));
        frame[5] = 0;
        assert_eq!(frame_info(&frame), (0x0102_0304_0506_0708, false));
        // Short frames are not valid frames; they count as projection 0.
        assert_eq!(frame_info(&frame[..12]), (0, false));
        assert_eq!(frame_info(&[]), (0, false));
    }

    #[test]
    fn a_projection_may_have_three_unfinished_frames_and_a_key_frame_may_make_four() {
        let budget = budget();
        let held: Vec<_> = (0..3)
            .map(|_| budget.reserve(7, false, 100).unwrap())
            .collect();
        assert_eq!(
            budget.reserve(7, false, 100).unwrap_err(),
            LinkError::Congested
        );
        // Another projection is counted separately.
        let other = budget.reserve(8, false, 100).unwrap();
        // A key frame goes past the count.
        let key = budget.reserve(7, true, 100).unwrap();
        // ...and then a delta is still refused.
        assert!(budget.reserve(7, false, 1).is_err());
        drop(held);
        drop(key);
        assert!(budget.reserve(7, false, 100).is_ok());
        drop(other);
    }

    #[test]
    fn bytes_are_capped_per_peer_even_for_key_frames_and_return_when_frames_finish() {
        let budget = budget();
        let first = budget.reserve(1, true, MAX_UNFINISHED_BYTES - 10).unwrap();
        let second = budget.reserve(2, false, 10).unwrap();
        assert_eq!(
            budget.reserve(3, true, 1).unwrap_err(),
            LinkError::Congested
        );
        assert!(budget.reserve(3, false, 1).is_err());
        drop(first);
        assert!(budget.reserve(3, true, MAX_UNFINISHED_BYTES - 10).is_ok());
        drop(second);
    }

    #[test]
    fn a_dropped_reservation_leaves_no_bookkeeping_behind() {
        let budget = budget();
        for projection in 0..100 {
            drop(budget.reserve(projection, false, 5).unwrap());
        }
        let state = lock(&budget.state);
        assert_eq!(state.bytes, 0);
        assert!(state.frames.is_empty());
    }

    #[test]
    fn partly_received_bytes_are_counted_and_released() {
        let total = Arc::new(AtomicUsize::new(0));
        let mut one = Buffered {
            total: total.clone(),
            mine: 0,
        };
        let mut two = Buffered {
            total: total.clone(),
            mine: 0,
        };
        assert!(one.add(MAX_RECEIVING_BYTES - 5));
        assert!(!two.add(6));
        drop(one);
        drop(two);
        assert_eq!(total.load(Ordering::Relaxed), 0);
    }
}
