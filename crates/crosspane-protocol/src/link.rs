//! The engine's view of one authenticated peer connection.
//!
//! Implemented by `crosspane-transport` (QUIC, 03 §2) and `crosspane-testkit` (simulated network).
//! The engine never sees bytes, streams or sockets.

use std::sync::Arc;
use std::time::Duration;

use crosspane_types::id::NodeId;

use crate::msg::{ControlMessage, InputMessage, PointerMessage};

/// Why a link failed or closed.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LinkError {
    /// The connection is closed.
    Closed,
    /// The message is too large or otherwise can't be encoded.
    Invalid(&'static str),
    /// The send queue is full. Only motion may be dropped this way; callers treat it as a
    /// congestion signal, not a failure.
    Congested,
}

/// One connected, authenticated peer.
pub trait PeerLink: Send {
    /// The peer's identity, from its pinned key.
    fn peer(&self) -> NodeId;

    /// Send on the input-discrete stream: reliable, ordered, highest priority.
    fn send_input(&mut self, msg: &InputMessage) -> Result<(), LinkError>;

    /// Send a pointer datagram: unreliable, latest wins. May return [`LinkError::Congested`].
    fn send_motion(&mut self, msg: &PointerMessage) -> Result<(), LinkError>;

    /// Send on the control stream: reliable, ordered.
    fn send_control(&mut self, msg: &ControlMessage) -> Result<(), LinkError>;

    /// The current smoothed round-trip time, if measured.
    fn rtt(&self) -> Option<Duration>;

    /// The peer's address on the path the connection uses now, if known (for link
    /// classification, 03 §2).
    fn remote_addr(&self) -> Option<std::net::SocketAddr> {
        None
    }

    /// Close the connection. Further sends return [`LinkError::Closed`].
    fn close(&mut self, message: &str);
}

/// Something that happened on a link. Delivered in order per peer; input and control messages are
/// never dropped.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum LinkEvent {
    Input {
        peer: NodeId,
        msg: InputMessage,
    },
    Motion {
        peer: NodeId,
        msg: PointerMessage,
    },
    Control {
        peer: NodeId,
        msg: ControlMessage,
    },
    /// E2: one complete media frame (a `crosspane-media` CPF1 payload) from `peer`, delivered when
    /// its stream finished. Frames of one projection can complete out of order; the receiver
    /// orders them by the frame header's `seq`.
    Media {
        peer: NodeId,
        data: std::sync::Arc<[u8]>,
    },
    /// The link to `peer` closed or failed.
    Closed {
        peer: NodeId,
        error: LinkError,
    },
}

/// Receives [`LinkEvent`]s. Never blocks (see `crosspane_platform::EventSink` for the same rules).
pub type LinkEventSink = Arc<dyn Fn(LinkEvent) + Send + Sync>;
