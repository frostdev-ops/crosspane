//! Shared helpers: transports on loopback, event collection, and a raw QUIC client that speaks
//! just enough of the protocol to misbehave on purpose.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use crosspane_protocol::ALPN;
use crosspane_protocol::link::{LinkEvent, LinkEventSink};
use crosspane_protocol::msg::{ControlMessage, Hello, InputMessage};
use crosspane_protocol::wire::{
    FrameDecoder, MAX_CONTROL_PAYLOAD, decode_control, encode_control, encode_input,
};
use crosspane_security::identity::DeviceIdentity;
use crosspane_transport::{PinStore, Transport, TransportConfig};
use crosspane_types::hid::HidUsage;
use crosspane_types::id::{NodeId, SessionId};
use quinn::crypto::rustls::QuicClientConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{AlwaysResolvesClientRawPublicKeys, ResolvesClientCert};
use rustls::crypto::{
    CryptoProvider, WebPkiSupportedAlgorithms, aws_lc_rs, verify_tls13_signature_with_raw_key,
};
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, SubjectPublicKeyInfoDer,
    UnixTime,
};
use rustls::sign::CertifiedKey;
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::time::timeout;

/// How long a test waits for something that should happen promptly. Generous, so a slow or busy CI
/// machine doesn't fail a test that only needs more time; a pass never takes this long.
pub const WAIT: Duration = Duration::from_secs(10);

/// Pins exactly the keys it was built with.
pub struct Pins(HashMap<Vec<u8>, NodeId>);

impl Pins {
    pub fn of(identities: &[&DeviceIdentity]) -> Arc<Pins> {
        Arc::new(Pins(
            identities
                .iter()
                .map(|identity| (identity.spki().to_vec(), identity.node()))
                .collect(),
        ))
    }
}

impl PinStore for Pins {
    fn trusted(&self, spki: &[u8]) -> Option<NodeId> {
        self.0.get(spki).copied()
    }
}

pub fn identity() -> Arc<DeviceIdentity> {
    Arc::new(DeviceIdentity::generate().unwrap())
}

pub fn hello(name: &str) -> Hello {
    Hello {
        minor: 0,
        name: name.to_owned(),
        features: vec!["e1".to_owned()],
        displays: Vec::new(),
    }
}

pub fn loopback() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

/// A dual-stack bind: reachable over IPv4 (as `127.0.0.1`) and IPv6 (as `[::1]`) at once. Tests that
/// need two addresses of one node use these two loopback addresses, because 127.0.0.2 and friends
/// only exist on Linux (macOS configures just 127.0.0.1 on lo0).
pub fn dual_stack() -> SocketAddr {
    SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0))
}

/// The two loopback addresses of a dual-stack node listening on `port`.
pub fn both_loopbacks(port: u16) -> (SocketAddr, SocketAddr) {
    (
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)),
    )
}

/// Whether this machine can run a dual-stack test: IPv6 loopback must exist. Prints why a test is
/// being skipped otherwise.
pub fn dual_stack_available(test: &str) -> bool {
    let works = std::net::UdpSocket::bind((std::net::Ipv6Addr::LOCALHOST, 0)).is_ok();
    if !works {
        eprintln!("SKIPPED {test}: no IPv6 loopback ([::1]) on this machine");
    }
    works
}

/// The `t0` of the Ping a node started with [`Node::start_replying`] sends from its Hello handler.
pub const REPLY_PING: u64 = 4242;

/// One transport plus the events it reported.
pub struct Node {
    pub transport: Arc<Transport>,
    pub identity: Arc<DeviceIdentity>,
    pub id: NodeId,
    pub events: UnboundedReceiver<LinkEvent>,
}

impl Node {
    pub fn start(name: &str, identity: Arc<DeviceIdentity>, pinned: &[&DeviceIdentity]) -> Node {
        Node::start_with(name, identity, Pins::of(pinned), loopback(), false)
    }

    /// Like [`Node::start`], and the event sink answers every `Hello` from inside the callback by
    /// fetching the peer's link and sending `Ping { t0: REPLY_PING }`: what an engine does.
    pub fn start_replying(
        name: &str,
        identity: Arc<DeviceIdentity>,
        pinned: &[&DeviceIdentity],
    ) -> Node {
        Node::start_with(name, identity, Pins::of(pinned), loopback(), true)
    }

    pub fn start_with(
        name: &str,
        identity: Arc<DeviceIdentity>,
        pins: Arc<dyn PinStore>,
        bind: SocketAddr,
        reply_to_hello: bool,
    ) -> Node {
        let (tx, events) = unbounded_channel();
        // The sink must not keep the transport alive, or dropping the node would never close it.
        let slot: Arc<OnceLock<Weak<Transport>>> = Arc::new(OnceLock::new());
        let sink: LinkEventSink = {
            let slot = slot.clone();
            Arc::new(move |event| {
                if reply_to_hello
                    && let LinkEvent::Control {
                        peer,
                        msg: ControlMessage::Hello(_),
                    } = &event
                    && let Some(transport) = slot.get().and_then(Weak::upgrade)
                    && let Some(mut link) = transport.link(*peer)
                {
                    let _ = link.send_control(&ControlMessage::Ping { t0: REPLY_PING });
                }
                let _ = tx.send(event);
            })
        };
        let transport = Arc::new(
            Transport::bind(
                TransportConfig {
                    bind,
                    identity: identity.clone(),
                    pins,
                    hello: hello(name),
                },
                sink,
            )
            .unwrap(),
        );
        slot.set(Arc::downgrade(&transport)).unwrap();
        Node {
            transport,
            id: identity.node(),
            identity,
            events,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.transport.local_addr()
    }

    /// The next event, failing the test if none arrives in time.
    pub async fn next(&mut self) -> LinkEvent {
        next_event(&mut self.events).await
    }

    /// Assert the first event is `Hello` from `peer` with the given device name.
    pub async fn expect_hello(&mut self, peer: NodeId, name: &str) {
        match self.next().await {
            LinkEvent::Control {
                peer: from,
                msg: ControlMessage::Hello(hello),
            } => {
                assert_eq!(from, peer, "hello from the wrong peer");
                assert_eq!(hello.name, name);
            }
            other => panic!("expected a hello, got {other:?}"),
        }
    }

    /// Assert nothing is reported for `duration`.
    pub async fn expect_quiet(&mut self, duration: Duration) {
        if let Ok(event) = timeout(duration, self.events.recv()).await {
            panic!("unexpected event: {event:?}");
        }
    }

    /// Assert the next event is a `Closed` for `peer` carrying `error`.
    pub async fn expect_closed(
        &mut self,
        peer: NodeId,
        error: crosspane_protocol::link::LinkError,
    ) {
        match self.next().await {
            LinkEvent::Closed {
                peer: from,
                error: got,
            } => {
                assert_eq!(from, peer);
                assert_eq!(got, error);
            }
            other => panic!("expected Closed, got {other:?}"),
        }
    }
}

pub async fn next_event(events: &mut UnboundedReceiver<LinkEvent>) -> LinkEvent {
    timeout(WAIT, events.recv())
        .await
        .expect("timed out waiting for an event")
        .expect("the event sink was dropped")
}

/// Two transports that pin each other.
pub fn pair() -> (Node, Node) {
    let a = identity();
    let b = identity();
    (
        Node::start("a", a.clone(), &[&b]),
        Node::start("b", b, &[&a]),
    )
}

/// Connect `a` to `b` and consume both `Hello`s.
pub async fn connected_pair() -> (Node, Node) {
    let (mut a, mut b) = pair();
    assert_eq!(a.transport.connect(b.addr()).await.unwrap(), b.id);
    a.expect_hello(b.id, "b").await;
    b.expect_hello(a.id, "a").await;
    (a, b)
}

/// A key press: always a "down", so the receiver's rate limit applies to it.
pub fn key_down(seq: u32) -> InputMessage {
    InputMessage::Key {
        session: SessionId(1),
        seq,
        usage: HidUsage::keyboard(4),
        down: true,
    }
}

/// Poll `condition` until it holds, failing the test after `WAIT`.
pub async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !condition() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub fn key(seq: u32) -> InputMessage {
    InputMessage::Key {
        session: SessionId(1),
        seq,
        usage: HidUsage::keyboard(4),
        down: seq % 2 == 1,
    }
}

// ---- a raw client -------------------------------------------------------------------------------

/// Accepts exactly the expected server key.
#[derive(Debug)]
struct ExpectKey {
    spki: Vec<u8>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for ExpectKey {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.spki {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("unexpected server key".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is disabled".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature_with_raw_key(
            message,
            &SubjectPublicKeyInfoDer::from(cert.as_ref()),
            dss,
            &self.algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ECDSA_NISTP256_SHA256]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

/// Advertises RFC 7250 raw public keys but sends an empty Certificate message, so a refusal tests
/// mandatory client authentication rather than certificate-type negotiation.
#[derive(Debug)]
struct NoRawClientKey;

impl ResolvesClientCert for NoRawClientKey {
    fn resolve(
        &self,
        _root_hint_subjects: &[&[u8]],
        _sigschemes: &[SignatureScheme],
    ) -> Option<Arc<CertifiedKey>> {
        None
    }

    fn has_certs(&self) -> bool {
        false
    }

    fn only_raw_public_keys(&self) -> bool {
        true
    }
}

fn certified_key(provider: &CryptoProvider, identity: &DeviceIdentity) -> Arc<CertifiedKey> {
    let der = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(identity.pkcs8().to_vec()));
    let signing_key = provider.key_provider.load_private_key(der).unwrap();
    Arc::new(CertifiedKey::new(
        vec![CertificateDer::from(identity.spki().to_vec())],
        signing_key,
    ))
}

/// A QUIC client that authenticates like a real node but is driven by hand.
pub struct Raw {
    pub endpoint: quinn::Endpoint,
}

impl Raw {
    pub fn new() -> Raw {
        Raw {
            endpoint: quinn::Endpoint::client(loopback()).unwrap(),
        }
    }

    /// Connect as `identity` to a server that presents `server`'s key, using `alpn`.
    pub async fn connect_with_alpn(
        &self,
        identity: &DeviceIdentity,
        server: &DeviceIdentity,
        addr: SocketAddr,
        alpn: &[u8],
    ) -> Result<quinn::Connection, quinn::ConnectionError> {
        let provider = Arc::new(aws_lc_rs::default_provider());
        let verifier = Arc::new(ExpectKey {
            spki: server.spki().to_vec(),
            algorithms: provider.signature_verification_algorithms,
        });
        let key = certified_key(&provider, identity);
        let mut tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_cert_resolver(Arc::new(AlwaysResolvesClientRawPublicKeys::new(key)));
        tls.alpn_protocols = vec![alpn.to_vec()];
        let mut config =
            quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls).unwrap()));
        let mut transport = quinn::TransportConfig::default();
        transport.keep_alive_interval(Some(Duration::from_secs(1)));
        config.transport_config(Arc::new(transport));
        timeout(
            WAIT,
            self.endpoint
                .connect_with(config, addr, "crosspane")
                .unwrap(),
        )
        .await
        .expect("raw handshake timed out")
    }

    /// Connect presenting no client key at all.
    pub async fn connect_without_key(
        &self,
        server: &DeviceIdentity,
        addr: SocketAddr,
    ) -> Result<quinn::Connection, quinn::ConnectionError> {
        let provider = Arc::new(aws_lc_rs::default_provider());
        let verifier = Arc::new(ExpectKey {
            spki: server.spki().to_vec(),
            algorithms: provider.signature_verification_algorithms,
        });
        let mut tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_cert_resolver(Arc::new(NoRawClientKey));
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let config = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls).unwrap()));
        let connecting = self
            .endpoint
            .connect_with(config, addr, "crosspane")
            .unwrap();
        let conn = timeout(WAIT, connecting)
            .await
            .expect("raw handshake timed out")?;
        // The handshake can complete on the client before the server rejects it.
        match timeout(WAIT, conn.closed()).await {
            Ok(error) => Err(error),
            Err(_) => Ok(conn),
        }
    }

    pub async fn connect(
        &self,
        identity: &DeviceIdentity,
        server: &DeviceIdentity,
        addr: SocketAddr,
    ) -> quinn::Connection {
        self.connect_with_alpn(identity, server, addr, ALPN)
            .await
            .expect("raw connection refused")
    }
}

/// An encoded `Hello` control frame.
pub fn hello_frame(name: &str) -> Vec<u8> {
    let mut frame = Vec::new();
    encode_control(&ControlMessage::Hello(hello(name)), &mut frame).unwrap();
    frame
}

pub fn key_frame(seq: u32) -> Vec<u8> {
    let mut frame = Vec::new();
    encode_input(&key(seq), &mut frame).unwrap();
    frame
}

/// Open a unidirectional stream, write its type byte and `bytes`, and keep it open.
pub async fn open_stream(conn: &quinn::Connection, kind: u8, bytes: &[u8]) -> quinn::SendStream {
    let mut send = conn.open_uni().await.unwrap();
    send.write_all(&[kind]).await.unwrap();
    send.write_all(bytes).await.unwrap();
    send
}

/// Like [`open_stream`], but `None` if opening or writing fails: a refusal can arrive before the
/// stream opens or while its bytes are being written. Callers must assert the expected close
/// separately.
pub async fn try_open_stream(
    conn: &quinn::Connection,
    kind: u8,
    bytes: &[u8],
) -> Option<quinn::SendStream> {
    let mut send = conn.open_uni().await.ok()?;
    send.write_all(&[kind]).await.ok()?;
    send.write_all(bytes).await.ok()?;
    Some(send)
}

/// Wait for the connection to close and return its application close code and reason.
pub async fn closed_by_peer(conn: &quinn::Connection) -> (u64, String) {
    match timeout(WAIT, conn.closed())
        .await
        .expect("connection never closed")
    {
        quinn::ConnectionError::ApplicationClosed(close) => (
            u64::from(close.error_code),
            String::from_utf8_lossy(&close.reason).into_owned(),
        ),
        other => panic!("expected an application close, got {other:?}"),
    }
}

/// A raw peer on a private runtime that can be killed outright: no close frame, no keep-alives, no
/// acknowledgements, like a crashed process.
pub struct CrashablePeer {
    crash: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CrashablePeer {
    /// Connect as `identity` and send a Hello. The peer stays alive (and keeps its connection
    /// healthy) until [`CrashablePeer::crash`].
    pub async fn connect(
        identity: Arc<DeviceIdentity>,
        server: Arc<DeviceIdentity>,
        addr: SocketAddr,
    ) -> CrashablePeer {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (crash_tx, crash_rx) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let raw = Raw::new();
                let conn = raw.connect(&identity, &server, addr).await;
                let control = open_stream(&conn, 0x01, &hello_frame("crashed")).await;
                let _ = ready_tx.send(());
                let _ = crash_rx.await;
                // A crash runs no destructors.
                std::mem::forget(control);
                std::mem::forget(conn);
                std::mem::forget(raw);
            });
            // Dropping the runtime's tasks without polling them again: nothing more is sent.
            runtime.shutdown_background();
        });
        ready_rx.await.unwrap();
        CrashablePeer {
            crash: Some(crash_tx),
            thread: Some(thread),
        }
    }

    /// Kill the peer. Returns once it has stopped sending.
    pub async fn crash(mut self) {
        let _ = self.crash.take().unwrap().send(());
        let thread = self.thread.take().unwrap();
        tokio::task::spawn_blocking(move || thread.join().unwrap())
            .await
            .unwrap();
    }
}

/// A loopback node with an explicitly configured feature advertisement.
pub fn node_with_hello(
    identity: Arc<DeviceIdentity>,
    pinned: &[&DeviceIdentity],
    hello: Hello,
) -> Node {
    let (tx, events) = unbounded_channel();
    let transport = Arc::new(
        Transport::bind(
            TransportConfig {
                bind: loopback(),
                identity: identity.clone(),
                pins: Pins::of(pinned),
                hello,
            },
            Arc::new(move |event| {
                let _ = tx.send(event);
            }),
        )
        .unwrap(),
    );
    Node {
        transport,
        id: identity.node(),
        identity,
        events,
    }
}

/// A `Hello` advertising exactly `features`.
pub fn hello_with(name: &str, features: &[&str]) -> Hello {
    Hello {
        features: features
            .iter()
            .map(|feature| (*feature).to_owned())
            .collect(),
        ..hello(name)
    }
}

/// An encoded `Hello` control frame advertising exactly `features`.
pub fn hello_frame_with(name: &str, features: &[&str]) -> Vec<u8> {
    let mut frame = Vec::new();
    encode_control(
        &ControlMessage::Hello(hello_with(name, features)),
        &mut frame,
    )
    .unwrap();
    frame
}

/// An encoded control frame.
pub fn control_frame(msg: &ControlMessage) -> Vec<u8> {
    let mut frame = Vec::new();
    encode_control(msg, &mut frame).unwrap();
    frame
}

/// What a raw peer receives from the node it connected to: the node's control stream decoded
/// message by message. Both of the node's streams are kept open (dropping one would make the node
/// close the connection), and the `Hello` the node starts with is read by [`RawReceiver::accept`].
pub struct RawReceiver {
    control: quinn::RecvStream,
    _input: quinn::RecvStream,
    decoder: FrameDecoder,
}

impl RawReceiver {
    /// Accept the node's two streams and return the receiver together with the node's `Hello`.
    pub async fn accept(conn: &quinn::Connection) -> (RawReceiver, Hello) {
        let mut control = None;
        let mut input = None;
        for _ in 0..2 {
            let mut recv = timeout(WAIT, conn.accept_uni())
                .await
                .expect("the node never opened its streams")
                .unwrap();
            let mut kind = [0u8; 1];
            recv.read_exact(&mut kind).await.unwrap();
            match kind[0] {
                0x01 => control = Some(recv),
                0x02 => input = Some(recv),
                other => panic!("unexpected stream type {other:#x}"),
            }
        }
        let mut receiver = RawReceiver {
            control: control.expect("no control stream"),
            _input: input.expect("no input stream"),
            decoder: FrameDecoder::new(MAX_CONTROL_PAYLOAD),
        };
        match receiver.next().await {
            ControlMessage::Hello(hello) => (receiver, hello),
            other => panic!("expected the node's hello first, got {other:?}"),
        }
    }

    async fn read_message(&mut self) -> ControlMessage {
        loop {
            if let Some(frame) = self.decoder.next_frame().unwrap() {
                return decode_control(&frame).unwrap();
            }
            let chunk = self
                .control
                .read_chunk(4096, true)
                .await
                .unwrap()
                .expect("the control stream ended");
            self.decoder.push(&chunk.bytes);
        }
    }

    /// The next control message the node sent.
    pub async fn next(&mut self) -> ControlMessage {
        timeout(WAIT, self.read_message())
            .await
            .expect("timed out waiting for a control message")
    }

    /// Assert the node sends no control message for `duration`.
    pub async fn expect_quiet(&mut self, duration: Duration) {
        if let Ok(msg) = timeout(duration, self.read_message()).await {
            panic!("unexpected control message: {msg:?}");
        }
    }
}
