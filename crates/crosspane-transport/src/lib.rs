//! QUIC endpoint, pinned raw-public-key authentication, and the channel layout (03 §2, 04 §3).
//!
//! One QUIC connection per peer pair, mutually authenticated with pinned P-256 device keys (TLS
//! 1.3, RFC 7250 raw public keys, ALPN `crosspane/1`, no 0-RTT, no resumption). Each side opens
//! two unidirectional streams, `control` (priority 50) and `input` (priority 100), and pointer
//! motion travels as datagrams. E2 media frames each get a unidirectional stream of their own
//! (type `0x03`, priority 10) through [`Transport::send_media`]. The engine sees all of it as a
//! [`PeerLink`] per peer plus [`LinkEvent`](crosspane_protocol::link::LinkEvent)s.
//!
//! Discovery, link classification, path selection, heartbeats and clock-offset estimation are
//! later work packages.

#![deny(unsafe_code)]

pub mod discovery;
mod hub;
mod link;
mod media;
pub mod pairing;
mod session;
mod tls;

// The unit tests need the transport's internals but share the integration tests' helpers, which
// name this crate by its external name.
#[cfg(test)]
extern crate self as crosspane_transport;
#[cfg(test)]
#[path = "../tests/common/mod.rs"]
mod common;
#[cfg(test)]
mod tests;

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crosspane_protocol::link::{LinkError, LinkEventSink, PeerLink};
use crosspane_protocol::msg::{ControlMessage, Hello};
use crosspane_protocol::wire::encode_control;
use crosspane_security::identity::DeviceIdentity;
use crosspane_types::id::NodeId;
use quinn::{IdleTimeout, VarInt};

use crate::hub::Inner;

/// The default UDP port.
pub const DEFAULT_PORT: u16 = 47_811;

/// Keep-alive pings keep NAT bindings open and detect a dead peer quickly (03 §2).
const KEEP_ALIVE: Duration = Duration::from_secs(1);
/// A peer silent for this long is dead.
const IDLE_TIMEOUT_MS: u32 = 10_000;
/// Streams a peer may have open to us. Two are long-lived (control and input); the rest carry one
/// media frame each. A few more than two are allowed so a stray control or input stream is seen
/// and refused as a protocol error instead of silently stalling.
const MAX_PEER_UNI_STREAMS: u8 = 64;
/// What a peer may send on one stream before we read it: the largest media frame.
const STREAM_RECEIVE_WINDOW: u32 = 64 * 1024 * 1024;
/// What a peer may send on the whole connection before we read it. Larger than the unfinished
/// media a peer may have (64 MiB) plus everything control and input can queue, so media alone can
/// never use up the connection's credit and stall them.
const CONNECTION_RECEIVE_WINDOW: u32 = 96 * 1024 * 1024;
/// What we may buffer, unacknowledged, across all our streams: the same reasoning on the sending
/// side. quinn's default (about 10 MB) is shared by every stream, so a few large frames would
/// make a control or input write wait for them to be acknowledged.
const SEND_WINDOW: u64 = 80 * 1024 * 1024;
/// Datagram buffers are small on purpose: a stale pointer position is worth less than a fresh one.
const DATAGRAM_RECEIVE_BUFFER: usize = 64 * 1024;
const DATAGRAM_SEND_BUFFER: usize = 16 * 1024;

/// The trust check the TLS verifiers use. The agent implements it over `TrustStore::trusted`.
///
/// It runs inside the TLS handshake, so it must be quick and must not block.
pub trait PinStore: Send + Sync + 'static {
    /// The peer's NodeId if this SPKI is pinned and not revoked.
    fn trusted(&self, spki: &[u8]) -> Option<NodeId>;
}

pub struct TransportConfig {
    pub bind: SocketAddr,
    pub identity: Arc<DeviceIdentity>,
    pub pins: Arc<dyn PinStore>,
    /// Sent as the first control message on every new connection.
    pub hello: Hello,
}

impl fmt::Debug for TransportConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransportConfig")
            .field("bind", &self.bind)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("bind failed: {0}")]
    Bind(std::io::Error),
    #[error("TLS setup failed: {0}")]
    Tls(String),
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("the peer's key is not trusted")]
    Untrusted,
    #[error("timed out")]
    Timeout,
}

pub struct Transport {
    inner: Arc<Inner>,
    local_addr: SocketAddr,
}

impl fmt::Debug for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Transport")
            .field("local_addr", &self.local_addr)
            .field("peers", &self.inner.peers().len())
            .finish()
    }
}

impl Transport {
    /// Bind the endpoint. Must be called inside a tokio runtime.
    /// - Incoming connections from pinned peers are accepted automatically; others fail the TLS
    ///   handshake.
    /// - Every event goes to `events`. The first event from a new peer is
    ///   `LinkEvent::Control { msg: ControlMessage::Hello(..) }`.
    ///
    /// Dropping the `Transport` closes every connection at once, without flushing; call
    /// [`Transport::shutdown`] first for a graceful stop.
    pub fn bind(
        config: TransportConfig,
        events: LinkEventSink,
    ) -> Result<Transport, TransportError> {
        let local_audio = config
            .hello
            .features
            .iter()
            .any(|feature| feature == "audio");
        let mut hello_frame = Vec::new();
        encode_control(&ControlMessage::Hello(config.hello), &mut hello_frame).map_err(
            |error| {
                TransportError::Bind(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("the hello message cannot be encoded: {error}"),
                ))
            },
        )?;

        let tls = tls::build(&config.identity, config.pins.clone())?;
        let mut quic = quinn::TransportConfig::default();
        quic.keep_alive_interval(Some(KEEP_ALIVE));
        quic.max_idle_timeout(Some(IdleTimeout::from(VarInt::from_u32(IDLE_TIMEOUT_MS))));
        // Everything rides on uni streams and datagrams; bidirectional streams are refused.
        quic.max_concurrent_bidi_streams(VarInt::from_u32(0));
        quic.max_concurrent_uni_streams(VarInt::from_u32(u32::from(MAX_PEER_UNI_STREAMS)));
        quic.stream_receive_window(VarInt::from_u32(STREAM_RECEIVE_WINDOW));
        quic.receive_window(VarInt::from_u32(CONNECTION_RECEIVE_WINDOW));
        quic.send_window(SEND_WINDOW);
        quic.datagram_receive_buffer_size(Some(DATAGRAM_RECEIVE_BUFFER));
        quic.datagram_send_buffer_size(DATAGRAM_SEND_BUFFER);
        let quic = Arc::new(quic);

        let mut server_config = quinn::ServerConfig::with_crypto(tls.server);
        server_config.transport_config(quic.clone());
        let mut client_config = quinn::ClientConfig::new(tls.client);
        client_config.transport_config(quic);

        // Fails with an I/O error if there is no tokio runtime.
        let endpoint =
            quinn::Endpoint::server(server_config, config.bind).map_err(TransportError::Bind)?;
        let local_addr = endpoint.local_addr().map_err(TransportError::Bind)?;

        let inner = Arc::new(Inner::new(
            endpoint,
            config.identity.node(),
            config.pins,
            events,
            hello_frame,
            client_config,
            local_audio,
        ));
        tokio::spawn(inner.clone().accept_loop());
        Ok(Transport { inner, local_addr })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Connect to `addr` and return the peer once it is authenticated and its link is usable
    /// (`link` returns it and its `Hello` is on the way). If a live connection to that peer already
    /// exists, return its NodeId without opening another. 5 s timeout.
    ///
    /// Concurrent calls for one address share a single attempt. A connection whose client has the
    /// larger NodeId of the pair is held for at least 300 ms before it is announced (longer while
    /// the peer's own dial is still being set up), in case the peer dials at the same moment, so
    /// `connect` can take that long; the engine then hears of one connection only.
    pub async fn connect(&self, addr: SocketAddr) -> Result<NodeId, TransportError> {
        self.inner.connect(addr).await
    }

    /// A send handle for a connected peer; `None` if not connected.
    pub fn link(&self, peer: NodeId) -> Option<Box<dyn PeerLink>> {
        self.inner.link(peer)
    }

    /// Queue one E2 media frame to `peer` on a new unidirectional stream (type byte `0x03`, then
    /// the frame, then FIN), below control and input in priority. Never blocks, and the stream is
    /// written by a task, so the frame's `Arc` is held until the peer has acknowledged all of it.
    ///
    /// The frame is a `crosspane-media` CPF1 payload; only its projection id and key-frame flag
    /// are read here, to apply the limits below. At most 3 frames per projection and 64 MiB of
    /// media per peer may be unfinished (not yet fully received by the peer) at once.
    /// - `Err(LinkError::Closed)` if there is no live connection to `peer`.
    /// - `Err(LinkError::Congested)` if accepting the frame would exceed those limits. It is
    ///   **not** sent, and the caller must make its next frame a key frame: a dropped delta would
    ///   corrupt the receiver's canvas. A key frame may exceed the per-projection count (never the
    ///   byte limit), so one can always follow once the bytes drain.
    /// - `Err(LinkError::Invalid(..))` for a frame over 64 MiB, which the peer would refuse; no
    ///   frame that large ever succeeds.
    ///
    /// Frames of one projection can arrive out of order, and a frame still unfinished when the
    /// connection closes is lost without notice. The receiver gets each complete frame as
    /// [`LinkEvent::Media`](crosspane_protocol::link::LinkEvent::Media).
    pub fn send_media(&self, peer: NodeId, frame: Arc<[u8]>) -> Result<(), LinkError> {
        self.inner.send_media(peer, frame)
    }

    /// Peers with a live connection, sorted.
    pub fn peers(&self) -> Vec<NodeId> {
        self.inner.peers()
    }

    /// Close every connection with `message` and stop accepting. Everything already queued on a
    /// link is flushed first (for up to a quarter of a second).
    pub async fn shutdown(&self, message: &str) {
        self.inner.shutdown(message).await;
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        // Closes every connection and stops the accept loop. Each peer sees a normal close. This
        // cannot flush queued data: use `shutdown` first for that.
        self.inner.stop();
    }
}
