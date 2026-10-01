//! The connection registry: who is connected, which connection carries a peer's logical link,
//! duplicate resolution, and the single place events reach the engine.
//!
//! # Duplicate connections
//!
//! Two nodes can end up with more than one connection to each other: both dial at once, one
//! dials a second address, or a restarted peer dials while the old connection is still held. The
//! rules, applied identically wherever a connection is registered:
//!
//! * **Same node dialed both** (same client): the *server* decides. A healthy old connection
//!   wins and the newcomer is refused with code 2 (the client sees the code and returns the
//!   existing peer). A silent or dead old connection (no packet for 1.5 s, or closed) is replaced:
//!   the engine sees `Closed`, then a new `Hello`.
//! * **Opposite directions, close together** (the old connection is under 3 s old): rule 7. The
//!   connection whose client has the smaller NodeId survives; the other closes with code 2,
//!   silently.
//! * **Opposite directions, far apart**: not a simultaneous connect. A healthy old connection wins
//!   (the newcomer is refused); a silent or dead one is replaced.
//!
//! A connection that can still lose rule 7 (its client has the larger NodeId of the pair) is
//! *held*: it is registered but unannounced for [`SETTLE`], and for as long after that as a
//! handshake with the same address is still in progress in either direction (a competing dial).
//! So the engine never sees a `Hello` from a connection that is about to be closed as a duplicate,
//! and never replies on one.
//!
//! # Silent supersession
//!
//! When a connection takes over a logical link the engine already knows (rule 7, or a peer that
//! declared the old connection a duplicate), the engine is told neither `Closed` nor a new
//! `Hello`. The survivor's own first Hello, which may advertise different features, is delivered as
//! [`LinkEvent::HelloRefresh`] before any later event of that connection. Each logical link has
//! exactly one ordinary `Hello`, and refused or replaced connections deliver nothing at all.
//!
//! # Locking
//!
//! Two mutexes, always taken in this order, never held across an `.await`:
//! 1. `emit` serializes every call into the event sink. Together with the registry check it makes
//!    "is this connection still current, and if so deliver" one atomic step, so the engine sees a
//!    peer's events in a consistent order (`Closed` before a replacement's `Hello`).
//! 2. `peers` is the registry itself. It is released before the sink runs, so a sink may call
//!    back into [`Transport`](crate::Transport) (`link`, `peers`) without deadlocking.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crosspane_protocol::link::{LinkError, LinkEvent, LinkEventSink, PeerLink};
use crosspane_protocol::msg::{ControlMessage, Hello};
use crosspane_security::identity::node_id;
use crosspane_types::id::NodeId;
use quinn::{Connection, ConnectionError, Endpoint, VarInt};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::link::{ConnTx, FLUSH_TIMEOUT, LinkCell, QuicLink};
use crate::session::{self, AcceptError, Start};
use crate::{PinStore, TransportError, tls};

/// How long `connect` may take in total.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a connection the peer closed as a duplicate waits for its replacement to arrive
/// before the engine is told the link closed.
pub(crate) const DUPLICATE_GRACE: Duration = Duration::from_secs(2);
/// How long a connection that can still lose rule 7 stays unannounced, at least: long enough for a
/// competing dial started at the same moment to show its first packet.
pub(crate) const SETTLE: Duration = Duration::from_millis(300);
/// The longest a connection is held while a competing handshake is still in progress.
pub(crate) const MAX_HOLD: Duration = Duration::from_secs(3);
/// Opposite-direction connections registered less than this far apart count as a simultaneous
/// connect and are ordered by rule 7.
const SIMULTANEOUS: Duration = Duration::from_secs(3);
/// A connection that has received nothing for this long is dead for duplicate purposes. (The peer
/// sends keep-alives every second.)
pub(crate) const STALE_SILENCE: Duration = Duration::from_millis(1_500);
/// How long `shutdown` waits for the close frames to leave.
const SHUTDOWN_FLUSH: Duration = Duration::from_secs(2);
/// Unauthenticated handshakes allowed in flight at once.
pub(crate) const MAX_HANDSHAKES: usize = 16;
/// How long one incoming handshake may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// After a retry was sent, the client is expected back within this long.
const RETRY_COMES_BACK: Duration = Duration::from_secs(1);
/// Protocol faults are logged at warn level at most this often; the rest go to debug.
const FAULT_LOG_INTERVAL: Duration = Duration::from_secs(1);

/// QUIC application error codes.
pub(crate) const CODE_NORMAL: u32 = 0;
pub(crate) const CODE_PROTOCOL_ERROR: u32 = 1;
pub(crate) const CODE_DUPLICATE: u32 = 2;
/// The peer stopped reading and our send queue hit its cap.
pub(crate) const CODE_OVERFLOW: u32 = 3;

/// TLS alert numbers that mean "the peer's key was refused" (RFC 8446 §6.2). QUIC carries them as
/// crypto error codes `0x100 + alert`.
const UNTRUSTED_ALERTS: [u8; 9] = [42, 43, 44, 45, 46, 48, 49, 51, 116];
const ALERT_NO_APPLICATION_PROTOCOL: u8 = 120;

/// The one spelling of an address, so a peer is recognised whichever way it is written or reported:
/// an IPv4 address on a dual-stack (IPv6) endpoint shows up IPv4-mapped (`::ffff:a.b.c.d`).
pub(crate) fn canonical(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V4(_) => addr,
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(v4.into(), v6.port()),
            // Keep the scope (needed for link-local addresses); drop the flow label.
            None => SocketAddr::V6(std::net::SocketAddrV6::new(
                *v6.ip(),
                v6.port(),
                0,
                v6.scope_id(),
            )),
        },
    }
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    /// We dialed the connection.
    Client,
    /// The peer dialed us.
    Server,
}

/// When a connection last received a UDP datagram, as sampled by its reader task.
#[derive(Debug)]
pub(crate) struct Activity {
    epoch: Instant,
    last_ms: AtomicU64,
}

impl Activity {
    pub(crate) fn new() -> Arc<Activity> {
        Arc::new(Activity {
            epoch: Instant::now(),
            last_ms: AtomicU64::new(0),
        })
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    pub(crate) fn touch(&self) {
        self.last_ms.store(self.elapsed_ms(), Ordering::Relaxed);
    }

    pub(crate) fn silent_for(&self) -> Duration {
        Duration::from_millis(
            self.elapsed_ms()
                .saturating_sub(self.last_ms.load(Ordering::Relaxed)),
        )
    }
}

struct PeerEntry {
    conn_id: u64,
    conn: Connection,
    /// The node that dialed the current connection.
    client: NodeId,
    cell: Arc<LinkCell>,
    /// When the current connection was registered.
    placed_at: Instant,
    activity: Arc<Activity>,
    /// The connection is visible to the engine: not held for a possible duplicate.
    settled: bool,
    /// The engine has been given this logical link's `Hello`.
    announced: bool,
    /// The peer closed the current connection as a duplicate, so another connection to us is on
    /// its way and takes over this link silently.
    duplicate_closed: bool,
}

impl PeerEntry {
    fn live(&self) -> bool {
        self.conn.close_reason().is_none()
    }

    /// What `decide` needs to know about this entry.
    fn view(&self) -> Old {
        let closed = self.conn.close_reason();
        Old {
            client: self.client,
            live: closed.is_none(),
            declared_duplicate: self.duplicate_closed
                || closed.as_ref().is_some_and(is_duplicate_close),
            age: self.placed_at.elapsed(),
            silent: self.activity.silent_for(),
        }
    }
}

/// An existing connection to a peer, as seen when a second one arrives.
#[derive(Clone, Copy, Debug)]
struct Old {
    /// The node that dialed it.
    client: NodeId,
    live: bool,
    /// The peer closed it as a duplicate (or its reader has noted that).
    declared_duplicate: bool,
    age: Duration,
    /// How long since it last received anything.
    silent: Duration,
}

impl Old {
    /// Dead, or silent for so long the peer is presumed gone.
    fn stale(&self) -> bool {
        !self.live || self.silent >= STALE_SILENCE
    }
}

/// How a connection that arrives while one is registered for the same peer is treated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    /// The new connection takes over the existing logical link; the old one closes silently.
    Supersede,
    /// The existing logical link ends (with a `Closed` event if the engine knew it) and the new
    /// connection starts a fresh one.
    Replace,
    /// The new connection is the duplicate and closes silently.
    Reject,
}

/// Decide between an existing connection and a new one whose client is `client`.
fn decide(old: &Old, client: NodeId) -> Decision {
    let keep_or_replace = if old.stale() {
        Decision::Replace
    } else {
        Decision::Reject
    };
    if old.client == client {
        // The same node dialed twice. The server decides, and a healthy connection is not given
        // up for a redundant one.
        return keep_or_replace;
    }
    if old.declared_duplicate {
        // The peer itself declared the old connection a duplicate (so it holds another one: this).
        return Decision::Supersede;
    }
    if old.age >= SIMULTANEOUS {
        // Not a simultaneous connect: the old connection is established.
        return keep_or_replace;
    }
    if !old.live {
        Decision::Replace
    } else if client < old.client {
        // Rule 7: the connection whose client has the smaller NodeId survives.
        Decision::Supersede
    } else {
        Decision::Reject
    }
}

/// A connect failure that can be handed to every caller waiting on the same attempt.
#[derive(Clone, Debug)]
pub(crate) enum ConnectFail {
    Untrusted,
    Timeout,
    Connect(String),
}

impl ConnectFail {
    fn connect(why: impl Into<String>) -> ConnectFail {
        ConnectFail::Connect(why.into())
    }
}

impl From<ConnectFail> for TransportError {
    fn from(fail: ConnectFail) -> Self {
        match fail {
            ConnectFail::Untrusted => TransportError::Untrusted,
            ConnectFail::Timeout => TransportError::Timeout,
            ConnectFail::Connect(why) => TransportError::Connect(why),
        }
    }
}

/// An in-progress connect attempt: the channel its outcome arrives on.
type Flight = watch::Receiver<Option<Result<NodeId, ConnectFail>>>;

/// A claim on one of the in-flight handshake slots, released on drop.
pub(crate) struct HandshakeSlot(Arc<AtomicUsize>);

impl HandshakeSlot {
    pub(crate) fn acquire(counter: &Arc<AtomicUsize>, max: usize) -> Option<HandshakeSlot> {
        if counter.fetch_add(1, Ordering::AcqRel) >= max {
            counter.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(HandshakeSlot(counter.clone()))
    }
}

impl Drop for HandshakeSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Marks an incoming handshake from an address as in progress, until dropped.
struct OpenIncoming {
    open: Arc<Mutex<HashMap<SocketAddr, usize>>>,
    remote: SocketAddr,
}

impl OpenIncoming {
    fn new(open: &Arc<Mutex<HashMap<SocketAddr, usize>>>, remote: SocketAddr) -> OpenIncoming {
        *lock(open).entry(remote).or_insert(0) += 1;
        OpenIncoming {
            open: open.clone(),
            remote,
        }
    }
}

impl Drop for OpenIncoming {
    fn drop(&mut self) {
        let mut open = lock(&self.open);
        if let Some(count) = open.get_mut(&self.remote) {
            *count -= 1;
            if *count == 0 {
                open.remove(&self.remote);
            }
        }
    }
}

pub(crate) struct Inner {
    pub(crate) endpoint: Endpoint,
    local: NodeId,
    pins: Arc<dyn PinStore>,
    events: LinkEventSink,
    hello_frame: Vec<u8>,
    pub(crate) local_audio: bool,
    pub(crate) client_config: quinn::ClientConfig,
    next_conn_id: AtomicU64,
    emit: Mutex<()>,
    peers: Mutex<HashMap<NodeId, PeerEntry>>,
    /// Signalled whenever the registry changes.
    changed: Notify,
    /// Connect attempts in progress, by address: concurrent callers share one attempt.
    flights: Mutex<HashMap<SocketAddr, Flight>>,
    handshakes: Arc<AtomicUsize>,
    /// Incoming handshakes in progress, by the sender's address.
    incoming_open: Arc<Mutex<HashMap<SocketAddr, usize>>>,
    /// When a retry was last sent to an address: its client is about to come back.
    retried: Mutex<HashMap<SocketAddr, Instant>>,
    /// Retry packets sent to unvalidated senders.
    pub(crate) retries: AtomicU64,
    shutting_down: AtomicBool,
    last_fault_log: Mutex<Option<Instant>>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Inner { .. }")
    }
}

impl Inner {
    pub(crate) fn new(
        endpoint: Endpoint,
        local: NodeId,
        pins: Arc<dyn PinStore>,
        events: LinkEventSink,
        hello_frame: Vec<u8>,
        client_config: quinn::ClientConfig,
        local_audio: bool,
    ) -> Self {
        Self {
            endpoint,
            local,
            pins,
            events,
            hello_frame,
            local_audio,
            client_config,
            next_conn_id: AtomicU64::new(1),
            emit: Mutex::new(()),
            peers: Mutex::new(HashMap::new()),
            changed: Notify::new(),
            flights: Mutex::new(HashMap::new()),
            handshakes: Arc::new(AtomicUsize::new(0)),
            incoming_open: Arc::new(Mutex::new(HashMap::new())),
            retried: Mutex::new(HashMap::new()),
            retries: AtomicU64::new(0),
            shutting_down: AtomicBool::new(false),
            last_fault_log: Mutex::new(None),
        }
    }

    // ---- queries -----------------------------------------------------------------------------

    /// Peers whose link is visible to the engine and whose connection is open.
    pub(crate) fn peers(&self) -> Vec<NodeId> {
        let mut peers: Vec<NodeId> = lock(&self.peers)
            .iter()
            .filter(|(_, entry)| entry.settled && entry.live())
            .map(|(peer, _)| *peer)
            .collect();
        peers.sort();
        peers
    }

    pub(crate) fn link(&self, peer: NodeId) -> Option<Box<dyn PeerLink>> {
        let peers = lock(&self.peers);
        let entry = peers.get(&peer)?;
        if !entry.settled || !entry.live() {
            return None;
        }
        Some(Box::new(QuicLink::new(peer, entry.cell.clone())))
    }

    /// Queue a media frame to `peer`: `Closed` unless its link is visible and open.
    pub(crate) fn send_media(&self, peer: NodeId, frame: Arc<[u8]>) -> Result<(), LinkError> {
        let cell = {
            let peers = lock(&self.peers);
            let entry = peers.get(&peer).ok_or(LinkError::Closed)?;
            if !entry.settled || !entry.live() {
                return Err(LinkError::Closed);
            }
            entry.cell.clone()
        };
        cell.send_media(frame)
    }

    /// The peer behind an open connection (settled or not) whose remote address is `addr`.
    fn live_peer_at(&self, addr: SocketAddr) -> Option<NodeId> {
        lock(&self.peers)
            .iter()
            .find(|(_, entry)| entry.live() && canonical(entry.conn.remote_address()) == addr)
            .map(|(peer, _)| *peer)
    }

    /// Whether a handshake with `remote` is in progress, or about to be (we just sent it a retry),
    /// that may yet produce a competitor of a connection we hold. For a connection the peer dialed
    /// (`Role::Server`) our own dial of that address is the competitor; for one we dialed it is not
    /// (it is that very dial), so only the peer's incoming handshake counts.
    pub(crate) fn competitor_in_flight(&self, remote: SocketAddr, role: Role) -> bool {
        let remote = canonical(remote);
        (role == Role::Server && lock(&self.flights).contains_key(&remote))
            || lock(&self.incoming_open).contains_key(&remote)
            || lock(&self.retried)
                .get(&remote)
                .is_some_and(|at| at.elapsed() < RETRY_COMES_BACK)
    }

    pub(crate) fn shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Relaxed)
    }

    /// Log a protocol fault: at warn level at most once a second, else at debug level.
    pub(crate) fn log_fault(&self, peer: NodeId, reason: &'static str) {
        let now = Instant::now();
        let mut last = lock(&self.last_fault_log);
        if last.is_none_or(|at| now.duration_since(at) >= FAULT_LOG_INTERVAL) {
            *last = Some(now);
            tracing::warn!(peer = %peer.short(), reason, "protocol error; closing connection");
        } else {
            tracing::debug!(peer = %peer.short(), reason, "protocol error; closing connection");
        }
    }

    // ---- connecting --------------------------------------------------------------------------

    pub(crate) async fn accept_loop(self: Arc<Self>) {
        while let Some(incoming) = self.endpoint.accept().await {
            if self.shutting_down() {
                incoming.refuse();
                continue;
            }
            if !incoming.remote_address_validated() {
                // Make the sender prove it can receive at its claimed address before any handshake
                // state exists for it. Clients remember the token for later connections.
                self.retries.fetch_add(1, Ordering::Relaxed);
                {
                    let mut retried = lock(&self.retried);
                    let now = Instant::now();
                    retried.retain(|_, at| now.duration_since(*at) < RETRY_COMES_BACK);
                    retried.insert(canonical(incoming.remote_address()), now);
                }
                if incoming.retry().is_err() {
                    tracing::debug!("could not send a retry");
                }
                continue;
            }
            let Some(slot) = HandshakeSlot::acquire(&self.handshakes, MAX_HANDSHAKES) else {
                tracing::debug!("too many handshakes in flight; refusing one");
                incoming.refuse();
                continue;
            };
            let inner = self.clone();
            let opened =
                OpenIncoming::new(&self.incoming_open, canonical(incoming.remote_address()));
            tokio::spawn(async move {
                let handshake = timeout(HANDSHAKE_TIMEOUT, incoming).await;
                // Past the TLS handshake the peer is authenticated and no longer counts.
                drop(slot);
                drop(opened);
                match handshake {
                    Ok(Ok(conn)) => {
                        if let Err(error) = inner.adopt(conn, Role::Server).await {
                            tracing::debug!(?error, "incoming connection refused");
                        }
                    }
                    // Includes every peer the TLS verifier refused: no event, no registration.
                    Ok(Err(error)) => tracing::debug!(%error, "incoming handshake failed"),
                    Err(_) => tracing::debug!("incoming handshake timed out"),
                }
            });
        }
    }

    /// Connect to `addr`; concurrent calls for the same address share one attempt.
    pub(crate) async fn connect(
        self: &Arc<Self>,
        addr: SocketAddr,
    ) -> Result<NodeId, TransportError> {
        let addr = canonical(addr);
        if self.shutting_down() {
            return Err(TransportError::Connect(
                "the transport has shut down".into(),
            ));
        }
        if let Some(peer) = self.live_peer_at(addr) {
            self.wait_settled(peer, CONNECT_TIMEOUT).await?;
            return Ok(peer);
        }
        let mut flight = {
            let mut flights = lock(&self.flights);
            match flights.get(&addr) {
                Some(flight) => flight.clone(),
                None => {
                    let (tx, rx) = watch::channel(None);
                    flights.insert(addr, rx.clone());
                    let inner = self.clone();
                    // The attempt outlives any one caller, so a cancelled caller can't strand the
                    // others.
                    tokio::spawn(async move {
                        let result = match timeout(CONNECT_TIMEOUT, inner.attempt(addr)).await {
                            Ok(result) => result,
                            Err(_) => Err(ConnectFail::Timeout),
                        };
                        lock(&inner.flights).remove(&addr);
                        let _ = tx.send(Some(result));
                    });
                    rx
                }
            }
        };
        let outcome = flight
            .wait_for(|outcome| outcome.is_some())
            .await
            .map_err(|_| TransportError::Connect("the connection attempt was abandoned".into()))?
            .clone();
        outcome
            .unwrap_or_else(|| Err(ConnectFail::connect("no outcome")))
            .map_err(TransportError::from)
    }

    async fn attempt(self: &Arc<Self>, addr: SocketAddr) -> Result<NodeId, ConnectFail> {
        // Raw public keys carry no name; the verifier ignores it. The address keys quinn's cache of
        // address-validation tokens, so a repeat connection skips the retry round trip.
        let name = addr.ip().to_string();
        let connecting = self
            .endpoint
            .connect_with(self.client_config.clone(), addr, &name)
            .map_err(|error| ConnectFail::connect(error.to_string()))?;
        let conn = connecting.await.map_err(map_connection_error)?;
        self.adopt(conn, Role::Client).await
    }

    /// Close every connection gracefully (queued data is flushed), then stop the endpoint.
    pub(crate) async fn shutdown(&self, message: &str) {
        if !self.shutting_down.swap(true, Ordering::AcqRel) {
            let cells: Vec<Arc<LinkCell>> = lock(&self.peers)
                .values()
                .map(|entry| entry.cell.clone())
                .collect();
            let flushes: Vec<JoinHandle<()>> = cells
                .iter()
                .filter_map(|cell| cell.begin_close(message))
                .collect();
            let _ = timeout(FLUSH_TIMEOUT * 2, async {
                for flush in flushes {
                    let _ = flush.await;
                }
            })
            .await;
        }
        self.endpoint
            .close(VarInt::from_u32(CODE_NORMAL), message.as_bytes());
        let _ = timeout(SHUTDOWN_FLUSH, self.endpoint.wait_idle()).await;
    }

    /// Abruptly stop: refuse new connections and close the endpoint. For `Drop`.
    pub(crate) fn stop(&self) {
        self.shutting_down.store(true, Ordering::Release);
        self.endpoint
            .close(VarInt::from_u32(CODE_NORMAL), b"shutdown");
    }

    /// Take over an established, TLS-authenticated connection: identify the peer, resolve any
    /// duplicate, and start the connection's tasks. For a client, returns once the link is visible
    /// to the engine (or the peer already has one).
    async fn adopt(self: &Arc<Self>, conn: Connection, role: Role) -> Result<NodeId, ConnectFail> {
        let peer = match self.identify(&conn) {
            Ok(peer) => peer,
            Err(error) => {
                conn.close(VarInt::from_u32(CODE_NORMAL), b"untrusted");
                return Err(error);
            }
        };

        let (client, first) = match role {
            Role::Server => (peer, None),
            Role::Client => {
                if self.has_established_connection(peer) {
                    // We already hold a healthy connection to this peer (a second address of it,
                    // say). Keep it; the peer refuses this one too.
                    conn.close(VarInt::from_u32(CODE_DUPLICATE), b"duplicate");
                    self.wait_settled(peer, CONNECT_TIMEOUT).await?;
                    return Ok(peer);
                }
                // A client's TLS handshake can finish before the server has accepted the client's
                // key, so success is only known once the server opens its first stream. (The
                // server only opens streams after registering the connection.)
                match session::accept_stream(conn.clone()).await {
                    Ok((kind, recv)) => (self.local, Some((kind, recv))),
                    Err(AcceptError::Closed(error)) if is_duplicate_close(&error) => {
                        // The peer holds another connection to us and kept that one. Wait for it.
                        self.wait_settled(peer, DUPLICATE_GRACE).await?;
                        return Ok(peer);
                    }
                    Err(AcceptError::Closed(error)) => return Err(map_connection_error(error)),
                    Err(AcceptError::Fault(why)) => {
                        conn.close(VarInt::from_u32(CODE_PROTOCOL_ERROR), b"protocol error");
                        return Err(ConnectFail::connect(format!("protocol error: {why}")));
                    }
                }
            }
        };

        let (tx, control_rx, input_rx) = ConnTx::new(conn.clone());
        // The local Hello is the first frame on the control stream.
        tx.queue_control(self.hello_frame.clone())
            .map_err(|_| ConnectFail::connect("control channel closed"))?;
        let activity = Activity::new();
        activity.touch();
        let conn_id = self.next_conn_id.fetch_add(1, Ordering::Relaxed);
        let audio_enabled = tx.audio_enabled.clone();
        let Some(hold) = self.place(peer, conn_id, client, tx, activity.clone()) else {
            conn.close(VarInt::from_u32(CODE_DUPLICATE), b"duplicate");
            tracing::debug!(peer = %peer.short(), ?role, "duplicate connection refused");
            if role == Role::Client {
                self.wait_settled(peer, DUPLICATE_GRACE).await?;
            }
            return Ok(peer);
        };
        session::spawn(Start {
            inner: self.clone(),
            conn,
            conn_id,
            peer,
            control_rx,
            input_rx,
            first,
            hold,
            role,
            activity,
            audio_enabled,
        });
        if role == Role::Client {
            self.wait_settled(peer, CONNECT_TIMEOUT).await?;
        }
        Ok(peer)
    }

    /// The peer's NodeId from the SPKI the handshake just verified.
    fn identify(&self, conn: &Connection) -> Result<NodeId, ConnectFail> {
        let spki = tls::peer_spki(conn).ok_or(ConnectFail::Untrusted)?;
        let peer = self.pins.trusted(&spki).ok_or(ConnectFail::Untrusted)?;
        if peer != node_id(&spki) {
            // The trust store must name a key by its own hash.
            tracing::warn!("the pin store returned a NodeId that does not match the key; refusing");
            return Err(ConnectFail::Untrusted);
        }
        if peer == self.local {
            return Err(ConnectFail::connect(
                "refusing a connection from this node to itself",
            ));
        }
        Ok(peer)
    }

    /// Whether the registry already holds a healthy connection to `peer` that a new connection
    /// we dialed would only duplicate.
    fn has_established_connection(&self, peer: NodeId) -> bool {
        lock(&self.peers).get(&peer).is_some_and(|entry| {
            let old = entry.view();
            !old.declared_duplicate
                && !old.stale()
                && (old.client == self.local || old.age >= SIMULTANEOUS)
        })
    }

    /// Wait until `peer`'s link is visible to the engine.
    async fn wait_settled(&self, peer: NodeId, limit: Duration) -> Result<(), ConnectFail> {
        let wait = async {
            loop {
                // Created before the check, so a change in between still wakes us.
                let changed = self.changed.notified();
                match lock(&self.peers).get(&peer) {
                    Some(entry) if entry.settled && entry.live() => return Ok(()),
                    Some(_) => {}
                    None => {
                        return Err(ConnectFail::connect(
                            "the connection closed before it was established",
                        ));
                    }
                }
                changed.await;
            }
        };
        timeout(limit, wait).await.map_err(|_| {
            ConnectFail::connect("the peer's connection did not become available in time")
        })?
    }

    // ---- the registry ------------------------------------------------------------------------

    /// Register a connection for `peer`. `None` if it is the duplicate to refuse; otherwise whether
    /// it must be held unannounced until it can no longer lose rule 7.
    fn place(
        &self,
        peer: NodeId,
        conn_id: u64,
        client: NodeId,
        tx: ConnTx,
        activity: Arc<Activity>,
    ) -> Option<bool> {
        let _order = lock(&self.emit);
        let now = Instant::now();
        // It can lose rule 7 only if its client has the larger NodeId of the pair.
        let other = if client == self.local {
            peer
        } else {
            self.local
        };
        let can_lose = client > other;
        let mut ended: Option<PeerEntry> = None;
        let mut superseded: Option<Connection> = None;
        {
            let mut peers = lock(&self.peers);
            let decision = peers.get(&peer).map(|old| decide(&old.view(), client));
            let fresh = |cell: Arc<LinkCell>, settled: bool| PeerEntry {
                conn_id,
                conn: tx.conn.clone(),
                client,
                cell,
                placed_at: now,
                activity: activity.clone(),
                settled,
                announced: false,
                duplicate_closed: false,
            };
            match decision {
                None => {
                    peers.insert(peer, fresh(LinkCell::new(tx.clone()), !can_lose));
                }
                Some(Decision::Reject) => return None,
                Some(Decision::Replace) => {
                    ended = peers.insert(peer, fresh(LinkCell::new(tx.clone()), !can_lose));
                }
                Some(Decision::Supersede) => {
                    if let Some(entry) = peers.get_mut(&peer) {
                        entry.cell.set(tx.clone());
                        superseded = Some(std::mem::replace(&mut entry.conn, tx.conn.clone()));
                        entry.conn_id = conn_id;
                        entry.client = client;
                        entry.placed_at = now;
                        entry.activity = activity.clone();
                        entry.duplicate_closed = false;
                        // A link the engine already knows stays visible across the handover.
                        entry.settled = entry.settled || !can_lose;
                    }
                }
            }
        }
        if let Some(old) = superseded {
            old.close(VarInt::from_u32(CODE_DUPLICATE), b"duplicate");
        }
        if let Some(old) = ended {
            old.cell.clear();
            old.conn
                .close(VarInt::from_u32(CODE_DUPLICATE), b"duplicate");
            if old.settled {
                (self.events)(LinkEvent::Closed {
                    peer,
                    error: LinkError::Closed,
                });
            }
        }
        self.changed.notify_waiters();
        // A superseding connection inherits a link that is already visible, so only a connection
        // whose entry is still unsettled is held.
        let held = lock(&self.peers)
            .get(&peer)
            .is_some_and(|entry| entry.conn_id == conn_id && !entry.settled);
        Some(held)
    }

    /// The connection can no longer lose a duplicate race: make its link visible.
    pub(crate) fn settle(&self, peer: NodeId, conn_id: u64) {
        let mut peers = lock(&self.peers);
        if let Some(entry) = peers.get_mut(&peer)
            && entry.conn_id == conn_id
        {
            entry.settled = true;
        }
        drop(peers);
        self.changed.notify_waiters();
    }

    // ---- events ------------------------------------------------------------------------------

    /// Deliver `event` for `peer` if connection `conn_id` still carries the peer's link.
    pub(crate) fn deliver(&self, peer: NodeId, conn_id: u64, event: LinkEvent) {
        let _order = lock(&self.emit);
        let current = lock(&self.peers)
            .get(&peer)
            .is_some_and(|entry| entry.conn_id == conn_id);
        if current {
            (self.events)(event);
        }
    }

    /// Deliver a connection's first `Hello`, as the ordinary `Hello` if the engine has none for this
    /// logical link yet. If it already has one, `conn_id` is a connection that silently took the
    /// link over (a duplicate resolved in its favour): its Hello goes out as
    /// [`LinkEvent::HelloRefresh`], so the engine learns the survivor's features, and always before
    /// any later event of that connection (the session reads nothing else before its Hello). A
    /// connection that is no longer the link's current one (refused, replaced, or closed) delivers
    /// nothing, so a stale connection never produces a refresh.
    pub(crate) fn deliver_hello(&self, peer: NodeId, conn_id: u64, hello: Hello) {
        let _order = lock(&self.emit);
        let event = match lock(&self.peers).get_mut(&peer) {
            Some(entry) if entry.conn_id == conn_id => {
                Some(if std::mem::replace(&mut entry.announced, true) {
                    LinkEvent::HelloRefresh { peer, hello }
                } else {
                    LinkEvent::Control {
                        peer,
                        msg: ControlMessage::Hello(hello),
                    }
                })
            }
            _ => None,
        };
        if let Some(event) = event {
            (self.events)(event);
        }
    }

    /// Note that the peer closed connection `conn_id` as a duplicate. `true` if that connection
    /// still carries a link the engine knows, so a replacement is worth waiting for.
    pub(crate) fn mark_duplicate_closed(&self, peer: NodeId, conn_id: u64) -> bool {
        match lock(&self.peers).get_mut(&peer) {
            Some(entry) if entry.conn_id == conn_id && entry.settled => {
                entry.duplicate_closed = true;
                true
            }
            _ => false,
        }
    }

    /// Connection `conn_id` has closed: end the peer's link and tell the engine (if it knew the
    /// link), unless another connection already took the link over.
    pub(crate) fn finish(&self, peer: NodeId, conn_id: u64, error: LinkError) {
        let _order = lock(&self.emit);
        let ended = {
            let mut peers = lock(&self.peers);
            match peers.get(&peer) {
                Some(entry) if entry.conn_id == conn_id => peers.remove(&peer),
                _ => None,
            }
        };
        if let Some(entry) = ended {
            entry.cell.clear();
            if entry.settled {
                (self.events)(LinkEvent::Closed { peer, error });
            }
        }
        self.changed.notify_waiters();
    }
}

/// The peer closed the connection with the duplicate code.
pub(crate) fn is_duplicate_close(error: &ConnectionError) -> bool {
    matches!(error, ConnectionError::ApplicationClosed(close)
        if close.error_code == VarInt::from_u32(CODE_DUPLICATE))
}

/// Translate a failed connection attempt.
pub(crate) fn map_connection_error(error: ConnectionError) -> ConnectFail {
    let crypto_alert = match &error {
        ConnectionError::TransportError(error) => u64::from(error.code),
        ConnectionError::ConnectionClosed(close) => u64::from(close.error_code),
        ConnectionError::TimedOut => return ConnectFail::Timeout,
        _ => return ConnectFail::connect(error.to_string()),
    }
    .checked_sub(0x100)
    .and_then(|alert| u8::try_from(alert).ok());
    match crypto_alert {
        Some(alert) if UNTRUSTED_ALERTS.contains(&alert) => ConnectFail::Untrusted,
        Some(ALERT_NO_APPLICATION_PROTOCOL) => ConnectFail::connect(
            "the peer speaks an incompatible protocol version; update Crosspane on both nodes",
        ),
        _ => ConnectFail::connect(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{Node, Raw, hello, identity, wait_until};

    const LOW: NodeId = NodeId([1; 32]);
    const HIGH: NodeId = NodeId([9; 32]);

    fn old(client: NodeId, age_ms: u64, silent_ms: u64) -> Old {
        Old {
            client,
            live: true,
            declared_duplicate: false,
            age: Duration::from_millis(age_ms),
            silent: Duration::from_millis(silent_ms),
        }
    }

    #[test]
    fn the_same_client_dialing_twice_is_refused_while_the_old_connection_is_healthy() {
        for age_ms in [10, 2_000, 60_000] {
            assert_eq!(decide(&old(HIGH, age_ms, 100), HIGH), Decision::Reject);
            assert_eq!(decide(&old(LOW, age_ms, 100), LOW), Decision::Reject);
        }
    }

    #[test]
    fn the_same_client_replaces_a_silent_or_dead_old_connection_whatever_the_order() {
        for client in [LOW, HIGH] {
            for age_ms in [10, 2_000, 60_000] {
                assert_eq!(
                    decide(&old(client, age_ms, 1_500), client),
                    Decision::Replace
                );
                let mut dead = old(client, age_ms, 0);
                dead.live = false;
                assert_eq!(decide(&dead, client), Decision::Replace);
            }
        }
    }

    #[test]
    fn simultaneous_connects_in_opposite_directions_follow_rule_7() {
        // Registered close together: the smaller client survives, wherever each was registered.
        assert_eq!(decide(&old(HIGH, 20, 10), LOW), Decision::Supersede);
        assert_eq!(decide(&old(LOW, 20, 10), HIGH), Decision::Reject);
        assert_eq!(decide(&old(HIGH, 2_900, 10), LOW), Decision::Supersede);
        // A dead one in that window is simply replaced.
        let mut dead = old(LOW, 20, 0);
        dead.live = false;
        assert_eq!(decide(&dead, HIGH), Decision::Replace);
    }

    #[test]
    fn an_established_connection_is_not_overridden_by_node_id_order() {
        // Healthy and old: the newcomer is the duplicate, even if its client has the smaller id.
        assert_eq!(decide(&old(HIGH, 3_000, 100), LOW), Decision::Reject);
        assert_eq!(decide(&old(LOW, 60_000, 100), HIGH), Decision::Reject);
        // Silent and old: the peer restarted; the engine must see the restart.
        assert_eq!(decide(&old(HIGH, 3_000, 1_600), LOW), Decision::Replace);
        assert_eq!(decide(&old(LOW, 60_000, 5_000), HIGH), Decision::Replace);
    }

    #[test]
    fn a_connection_the_peer_declared_a_duplicate_is_taken_over_silently() {
        let mut declared = old(HIGH, 500, 0);
        declared.live = false;
        declared.declared_duplicate = true;
        assert_eq!(decide(&declared, LOW), Decision::Supersede);
        // ...but not if the same node simply dialed again.
        assert_eq!(decide(&declared, HIGH), Decision::Replace);
    }

    #[test]
    fn addresses_have_one_spelling_on_dual_stack_endpoints() {
        let v4: SocketAddr = "127.0.0.1:47811".parse().unwrap();
        let mapped: SocketAddr = "[::ffff:127.0.0.1]:47811".parse().unwrap();
        let v6: SocketAddr = "[::1]:47811".parse().unwrap();
        assert_eq!(canonical(v4), v4);
        assert_eq!(canonical(mapped), v4);
        assert_eq!(canonical(v6), v6);
        assert_ne!(canonical(v4), canonical(v6));
        let link_local: SocketAddr = "[fe80::1%3]:47811".parse().unwrap();
        assert_eq!(canonical(link_local), link_local);
    }

    #[test]
    fn handshake_slots_are_limited_and_released() {
        let counter = Arc::new(AtomicUsize::new(0));
        let slots: Vec<_> = (0..3)
            .map(|_| HandshakeSlot::acquire(&counter, 3).expect("a free slot"))
            .collect();
        assert!(HandshakeSlot::acquire(&counter, 3).is_none());
        assert_eq!(counter.load(Ordering::Relaxed), 3);
        drop(slots);
        assert_eq!(counter.load(Ordering::Relaxed), 0);
        assert!(HandshakeSlot::acquire(&counter, 3).is_some());
    }

    #[test]
    fn activity_reports_how_long_a_connection_has_been_silent() {
        let activity = Activity::new();
        activity.touch();
        assert!(activity.silent_for() < Duration::from_millis(100));
        std::thread::sleep(Duration::from_millis(60));
        assert!(activity.silent_for() >= Duration::from_millis(50));
        activity.touch();
        assert!(activity.silent_for() < Duration::from_millis(50));
    }

    /// A node with one real, registered connection from a raw peer, and that connection's id.
    async fn registered() -> (Node, Raw, quinn::Connection, NodeId, u64) {
        // The raw peer has the smaller NodeId, so its connection cannot lose rule 7 and is
        // visible to the engine at once (a larger client's would be held for a while first).
        let (one, two) = (identity(), identity());
        let (local, remote) = if one.node() > two.node() {
            (one, two)
        } else {
            (two, one)
        };
        let node = Node::start("node", local.clone(), &[&remote]);
        let peer = remote.node();
        let raw = Raw::new();
        let conn = raw.connect(&remote, &local, node.addr()).await;
        let inner = node.transport.inner.clone();
        wait_until("the connection is registered and visible", || {
            lock(&inner.peers)
                .get(&peer)
                .is_some_and(|entry| entry.settled)
        })
        .await;
        let current = lock(&inner.peers).get(&peer).map(|entry| entry.conn_id);
        (node, raw, conn, peer, current.unwrap())
    }

    /// Delivery re-checks, under the emit lock, that the connection is still the one carrying the
    /// peer's link. A connection the registry no longer holds (replaced, refused or closed, with
    /// traffic still buffered in its reader) must produce no event, no refresh, and no `Closed`,
    /// and must not use up the link's one ordinary `Hello`.
    #[tokio::test]
    async fn an_obsolete_connection_delivers_nothing_and_changes_nothing() {
        let (mut node, _raw, _conn, peer, current) = registered().await;
        let inner = node.transport.inner.clone();
        let ping = LinkEvent::Control {
            peer,
            msg: ControlMessage::Ping { t0: 1 },
        };
        // Ids the registry does not hold: never issued, and one issued after the current one.
        for obsolete in [0, current + 1, current + 1_000] {
            inner.deliver(peer, obsolete, ping.clone());
            inner.deliver(
                peer,
                obsolete,
                LinkEvent::HelloRefresh {
                    peer,
                    hello: hello("obsolete"),
                },
            );
            inner.deliver_hello(peer, obsolete, hello("obsolete"));
            inner.finish(peer, obsolete, LinkError::Closed);
        }
        // Every attempt returned, and not one of them reached the engine or touched the registry.
        node.expect_quiet(Duration::from_millis(100)).await;
        assert_eq!(
            lock(&inner.peers).get(&peer).map(|entry| entry.conn_id),
            Some(current)
        );

        // The obsolete attempts left the first-Hello state alone: the current connection's Hello
        // is still the ordinary one, then a refresh, and its other events pass.
        inner.deliver_hello(peer, current, hello("first"));
        node.expect_hello(peer, "first").await;
        inner.deliver_hello(peer, current + 1, hello("obsolete"));
        inner.deliver(peer, current + 1, ping.clone());
        node.expect_quiet(Duration::from_millis(100)).await;
        inner.deliver_hello(peer, current, hello("second"));
        match node.next().await {
            LinkEvent::HelloRefresh { peer: from, hello } => {
                assert_eq!(from, peer);
                assert_eq!(hello.name, "second");
            }
            other => panic!("expected a HelloRefresh, got {other:?}"),
        }
        inner.deliver(peer, current, ping.clone());
        assert_eq!(node.next().await, ping);
        node.expect_quiet(Duration::from_millis(100)).await;

        // Only the connection that is current can end the link.
        inner.finish(peer, current, LinkError::Closed);
        match node.next().await {
            LinkEvent::Closed { peer: from, .. } => assert_eq!(from, peer),
            other => panic!("expected Closed, got {other:?}"),
        }
        // A refresh or event from the now-gone connection is dropped as well.
        inner.deliver_hello(peer, current, hello("late"));
        inner.deliver(peer, current, ping);
        node.expect_quiet(Duration::from_millis(100)).await;
    }
}
