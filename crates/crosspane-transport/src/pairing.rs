//! Pairing connections: the network half of SAS pairing (04 §3, WP-1.3).
//!
//! Pairing runs the commit-reveal short-authentication-string exchange of
//! [`crosspane_security::pairing`] between two nodes that do not trust each other yet. It needs a
//! transport that carries *unauthenticated* peers, so it lives on its own endpoint and never mixes
//! with the normal [`Transport`](crate::Transport):
//!
//! * The listener exists only while the user has a pairing window open. The agent creates a
//!   [`PairingListener`] when the window opens and drops it when it closes.
//! * ALPN is `crosspane-pair/1` (the normal transport speaks `crosspane/1`), so neither endpoint
//!   will complete a handshake with the other. A pairing connection never produces a
//!   [`LinkEvent`](crosspane_protocol::link::LinkEvent) and is never a peer link.
//!
//! # TLS
//!
//! TLS 1.3 only, RFC 7250 raw public keys both ways, client authentication required, no 0-RTT and
//! no resumption: the same shape as the pinned transport (`tls.rs`). The difference is the
//! verifiers. Nothing is pinned yet, so they accept **any** well-formed P-256 SPKI. They still
//! verify the TLS 1.3 handshake signature (proof of possession: the peer really holds the private
//! key of the SPKI it presented) and allow only ECDSA P-256/SHA-256. TLS authenticates nothing
//! here. What authenticates the peer is the SAS, which binds both SPKIs, both nonces and the TLS
//! exporter of this very connection, and the user comparing the code. An attacker who terminates
//! TLS on both legs gets two different exporters and so two different codes.
//!
//! # The exporter
//!
//! [`PairingChannel::exporter`] is `quinn::Connection::export_keying_material` (RFC 5705 / RFC 8446
//! §7.5, i.e. rustls's keying material exporter) with the label `EXPORTER_LABEL`, an empty context
//! and `EXPORTER_LEN` bytes (both from `crosspane_security::pairing`). Both ends of one connection
//! derive the same value from the handshake's exporter secret; any other connection derives a
//! different one.
//!
//! # The stream
//!
//! The dialer opens one bidirectional stream and every message is one frame on it: the 8-byte
//! header of `crosspane_protocol::wire` with `kind = KIND_PAIRING` and `PairingMsg::encode()` as the
//! payload (at most 1024 bytes). Any other kind, an oversized payload or a payload that does not
//! decode closes the connection with application code 1.
//!
//! A QUIC stream is invisible to the peer until its opener sends a byte on it, but the initiator
//! (the listener side) speaks first. So the dialer's first frame is an empty `KIND_PAIRING` frame, a
//! *stream-open marker*. The listener consumes it before it hands out the channel; it is never a
//! message, and a later empty frame is a protocol error like any other undecodable payload.
//!
//! # Limits
//!
//! The listener runs at most 4 unauthenticated handshakes at once and gives each 5 s, including the
//! stream-open marker. A further handshake is refused before any state exists for it (QUIC cannot
//! carry an application code that early, so the dialer sees a plain refusal, not code 3). Completed
//! channels wait for `accept`, at most 4 of them; one more is closed with application code 3.
//! Connections are idle-closed after 120 s, flow-control windows are small, and no datagrams or
//! unidirectional streams are allowed.

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use crosspane_protocol::wire::{Frame, FrameDecoder, HEADER_LEN, KIND_PAIRING, WIRE_VERSION};
use crosspane_security::identity::{DeviceIdentity, point_from_spki};
use crosspane_security::pairing::{EXPORTER_LABEL, EXPORTER_LEN, PairingMsg};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{
    ConnectionError, Endpoint, IdleTimeout, ReadError, RecvStream, SendStream, VarInt, WriteError,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{AlwaysResolvesClientRawPublicKeys, Resumption};
use rustls::crypto::{
    CryptoProvider, WebPkiSupportedAlgorithms, aws_lc_rs, verify_tls13_signature_with_raw_key,
};
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, SubjectPublicKeyInfoDer,
    UnixTime,
};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::{AlwaysResolvesServerRawPublicKeys, NoServerSessionStorage};
use rustls::sign::CertifiedKey;
use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, Error, SignatureScheme};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::hub::HandshakeSlot;
use crate::{TransportError, tls};

/// The ALPN of pairing connections. The normal transport never offers or accepts it.
pub const PAIRING_ALPN: &[u8] = b"crosspane-pair/1";
/// The pairing listener's UDP port: the node's transport port + 1 (default 47_812).
pub const PAIRING_PORT_OFFSET: u16 = 1;

/// Handshakes (TLS and the stream-open marker) the listener runs at once.
const MAX_HANDSHAKES: usize = 4;
/// Completed channels that may wait for [`PairingListener::accept`]; more are closed with
/// `CODE_BUSY`.
const MAX_PENDING: usize = 4;
/// Largest `PairingMsg` payload, in bytes.
const MAX_PAYLOAD: usize = 1024;
/// How long one incoming handshake, up to and including the stream-open marker, may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `pair_connect` may take in total.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A connection with no traffic for this long is closed (the pairing window's length).
const IDLE_TIMEOUT_MS: u32 = 120_000;
/// How long `recv` waits for a message.
const RECV_TIMEOUT: Duration = Duration::from_secs(120);
/// How long `close` waits for the peer to acknowledge what was sent before closing.
const CLOSE_FLUSH: Duration = Duration::from_secs(1);
/// Per-stream and per-connection receive windows: pairing messages are tiny, and the peer is
/// unauthenticated.
const STREAM_WINDOW: u32 = 16 * 1024;
const CONNECTION_WINDOW: u32 = 64 * 1024;
const READ_CHUNK: usize = 2048;
/// The longest close reason sent to the peer, in bytes.
const MAX_REASON: usize = 128;

/// QUIC application error codes.
const CODE_NORMAL: u32 = 0;
const CODE_PROTOCOL_ERROR: u32 = 1;
/// A completed connection arrived while `MAX_PENDING` others were still waiting.
const CODE_BUSY: u32 = 3;

/// The only signature scheme Crosspane device keys use (P-256, 04 §2).
const SCHEME: SignatureScheme = SignatureScheme::ECDSA_NISTP256_SHA256;
const ALERT_NO_APPLICATION_PROTOCOL: u8 = 120;

// ---- TLS ----------------------------------------------------------------------------------------

/// Accepts any well-formed P-256 SPKI as the peer's raw public key and checks the handshake
/// signature with it (proof of possession) through rustls's raw-key verification.
///
/// One instance serves both roles: it is the client's [`ServerCertVerifier`] and the server's
/// [`ClientCertVerifier`]. Unlike the pinned verifier it has no trust decision to make: that is
/// the SAS exchange's job.
struct PairingVerifier {
    algorithms: WebPkiSupportedAlgorithms,
}

impl fmt::Debug for PairingVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairingVerifier { .. }")
    }
}

impl PairingVerifier {
    fn new(provider: &CryptoProvider) -> Self {
        Self {
            algorithms: provider.signature_verification_algorithms,
        }
    }

    /// A raw public key must be a bare, well-formed P-256 SPKI (a point on the curve): there is
    /// nothing else in the chain.
    fn check_key(
        spki: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
    ) -> Result<(), Error> {
        if !intermediates.is_empty() || point_from_spki(spki.as_ref()).is_err() {
            return Err(Error::InvalidCertificate(CertificateError::BadEncoding));
        }
        Ok(())
    }

    fn check_signature(
        &self,
        message: &[u8],
        spki: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        if signature.scheme != SCHEME {
            return Err(Error::General(
                "only P-256/SHA-256 signatures are allowed".into(),
            ));
        }
        verify_tls13_signature_with_raw_key(
            message,
            &SubjectPublicKeyInfoDer::from(spki.as_ref()),
            signature,
            &self.algorithms,
        )
    }
}

impl ServerCertVerifier for PairingVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Self::check_key(end_entity, intermediates)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("TLS 1.2 is disabled".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.check_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SCHEME]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

impl ClientCertVerifier for PairingVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    /// Both sides present a key: the SAS binds both SPKIs.
    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        Self::check_key(end_entity, intermediates)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("TLS 1.2 is disabled".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.check_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SCHEME]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

fn tls_error(error: impl fmt::Display) -> TransportError {
    TransportError::Tls(error.to_string())
}

/// Load the device key into the aws-lc-rs provider and wrap its SPKI as the raw public key the
/// handshake presents.
fn load_certified_key(
    provider: &CryptoProvider,
    identity: &DeviceIdentity,
) -> Result<Arc<CertifiedKey>, TransportError> {
    // The provider needs an owned, 'static copy of the PKCS#8 bytes (rustls does not zeroize it).
    let der = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(identity.pkcs8().to_vec()));
    let signing_key = provider
        .key_provider
        .load_private_key(der)
        .map_err(tls_error)?;
    let spki = identity.spki();
    let matches = signing_key
        .public_key()
        .is_some_and(|public| public.as_ref() == spki);
    if !matches {
        return Err(TransportError::Tls(
            "the provider and the device identity disagree on the public key".into(),
        ));
    }
    Ok(Arc::new(CertifiedKey::new(
        vec![CertificateDer::from(spki.to_vec())],
        signing_key,
    )))
}

/// The server TLS configuration: TLS 1.3 only, ALPN `crosspane-pair/1`, a raw public key, client
/// authentication required, no 0-RTT and no session resumption.
fn server_tls_with(
    provider: Arc<CryptoProvider>,
    key: Arc<CertifiedKey>,
) -> Result<rustls::ServerConfig, TransportError> {
    let verifier = Arc::new(PairingVerifier::new(&provider));
    let mut server = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(tls_error)?
        .with_client_cert_verifier(verifier)
        .with_cert_resolver(Arc::new(AlwaysResolvesServerRawPublicKeys::new(key)));
    server.alpn_protocols = vec![PAIRING_ALPN.to_vec()];
    // 0-RTT off: a zero early-data limit makes the server refuse early data (quinn requires exactly
    // 0 or u32::MAX here).
    server.max_early_data_size = 0;
    // Resumption off: issue no tickets, keep no session storage, and use no ticketer.
    server.send_tls13_tickets = 0;
    server.max_tls13_tickets = 0;
    server.session_storage = Arc::new(NoServerSessionStorage {});
    if server.ticketer.enabled() {
        return Err(TransportError::Tls("the server ticketer is enabled".into()));
    }
    Ok(server)
}

/// The client TLS configuration: the mirror image of [`server_tls_with`].
fn client_tls_with(
    provider: Arc<CryptoProvider>,
    key: Arc<CertifiedKey>,
) -> Result<rustls::ClientConfig, TransportError> {
    let verifier = Arc::new(PairingVerifier::new(&provider));
    let mut client = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(tls_error)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_cert_resolver(Arc::new(AlwaysResolvesClientRawPublicKeys::new(key)));
    client.alpn_protocols = vec![PAIRING_ALPN.to_vec()];
    // 0-RTT off on the client too, and no stored sessions or tickets: every connection is a full
    // handshake.
    client.enable_early_data = false;
    client.resumption = Resumption::disabled();
    Ok(client)
}

fn server_tls(identity: &DeviceIdentity) -> Result<rustls::ServerConfig, TransportError> {
    // Select aws-lc-rs explicitly rather than relying on a process-wide default provider.
    let provider = Arc::new(aws_lc_rs::default_provider());
    let key = load_certified_key(&provider, identity)?;
    server_tls_with(provider, key)
}

fn client_tls(identity: &DeviceIdentity) -> Result<rustls::ClientConfig, TransportError> {
    let provider = Arc::new(aws_lc_rs::default_provider());
    let key = load_certified_key(&provider, identity)?;
    client_tls_with(provider, key)
}

/// QUIC transport parameters. `peer_bidi_streams` is how many bidirectional streams the *peer* may
/// open: one on the listener (the dialer's pairing stream), none on the dialer.
fn transport_config(peer_bidi_streams: u32) -> Arc<quinn::TransportConfig> {
    let mut quic = quinn::TransportConfig::default();
    quic.max_idle_timeout(Some(IdleTimeout::from(VarInt::from_u32(IDLE_TIMEOUT_MS))));
    quic.max_concurrent_bidi_streams(VarInt::from_u32(peer_bidi_streams));
    quic.max_concurrent_uni_streams(VarInt::from_u32(0));
    // No pointer motion here.
    quic.datagram_receive_buffer_size(None);
    quic.stream_receive_window(VarInt::from_u32(STREAM_WINDOW));
    quic.receive_window(VarInt::from_u32(CONNECTION_WINDOW));
    quic.send_window(u64::from(CONNECTION_WINDOW));
    Arc::new(quic)
}

// ---- errors -------------------------------------------------------------------------------------

/// Translate a failed connection or stream operation.
fn connect_error(error: ConnectionError) -> TransportError {
    let crypto_alert = match &error {
        ConnectionError::TransportError(error) => u64::from(error.code),
        ConnectionError::ConnectionClosed(close) => u64::from(close.error_code),
        ConnectionError::TimedOut => return TransportError::Timeout,
        _ => return TransportError::Connect(error.to_string()),
    }
    .checked_sub(0x100)
    .and_then(|alert| u8::try_from(alert).ok());
    match crypto_alert {
        // QUIC carries TLS alerts as crypto error codes `0x100 + alert` (RFC 9001 §4.8).
        Some(ALERT_NO_APPLICATION_PROTOCOL) => TransportError::Connect(
            "the peer does not accept pairing connections (is a pairing window open there?)".into(),
        ),
        _ => TransportError::Connect(error.to_string()),
    }
}

fn read_error(error: ReadError) -> TransportError {
    match error {
        ReadError::ConnectionLost(error) => connect_error(error),
        other => TransportError::Connect(format!("the pairing stream failed: {other}")),
    }
}

fn write_error(error: WriteError) -> TransportError {
    match error {
        WriteError::ConnectionLost(error) => connect_error(error),
        other => TransportError::Connect(format!("the pairing stream failed: {other}")),
    }
}

// ---- framing ------------------------------------------------------------------------------------

/// A `KIND_PAIRING` frame around `payload` (at most [`MAX_PAYLOAD`] bytes).
fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, TransportError> {
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|_| payload.len() <= MAX_PAYLOAD)
        .ok_or_else(|| TransportError::Connect("the pairing message is too large".into()))?;
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.extend_from_slice(&[WIRE_VERSION, KIND_PAIRING, 0, 0]);
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// The TLS-derived facts of a completed handshake: the peer's SPKI and the exporter.
///
/// Closes the connection if either is unavailable.
fn session_facts(
    conn: &quinn::Connection,
) -> Result<(Vec<u8>, [u8; EXPORTER_LEN]), TransportError> {
    let fail = |why: &str| {
        conn.close(VarInt::from_u32(CODE_PROTOCOL_ERROR), b"protocol error");
        TransportError::Tls(why.into())
    };
    let Some(spki) = tls::peer_spki(conn) else {
        return Err(fail("the peer presented no raw public key"));
    };
    // The verifier already insisted on this; the SAS and the pin depend on it.
    if point_from_spki(&spki).is_err() {
        return Err(fail("the peer's key is not a P-256 SPKI"));
    }
    let mut exporter = [0; EXPORTER_LEN];
    if conn
        .export_keying_material(&mut exporter, EXPORTER_LABEL, b"")
        .is_err()
    {
        return Err(fail("the TLS keying material exporter is unavailable"));
    }
    Ok((spki, exporter))
}

// ---- the channel --------------------------------------------------------------------------------

/// One pairing exchange over one bidirectional stream.
///
/// The peer is unauthenticated: only the SAS exchange authenticates it. In particular
/// `PairingMsg::Reveal` carries a *claimed* SPKI; the caller must check that the SPKI the exchange
/// pins equals [`peer_spki`](PairingChannel::peer_spki), the key the peer proved possession of in
/// the handshake.
///
/// `send` is not cancel-safe: abandon a channel whose `send` was cancelled. `recv` is.
pub struct PairingChannel {
    conn: quinn::Connection,
    /// Keeps the UDP socket serving this connection for as long as the channel lives.
    _endpoint: Endpoint,
    send: SendStream,
    recv: RecvStream,
    decoder: FrameDecoder,
    peer_spki: Vec<u8>,
    exporter: [u8; EXPORTER_LEN],
    recv_timeout: Duration,
}

impl fmt::Debug for PairingChannel {
    // Neither the exporter nor the peer's key belong in diagnostics.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingChannel")
            .field("remote_address", &self.conn.remote_address())
            .finish_non_exhaustive()
    }
}

impl Drop for PairingChannel {
    fn drop(&mut self) {
        // Best effort: no `zeroize` here, so overwrite and keep the write observable.
        self.exporter = [0; EXPORTER_LEN];
        std::hint::black_box(&self.exporter);
    }
}

impl PairingChannel {
    fn new(
        conn: quinn::Connection,
        endpoint: Endpoint,
        stream: (SendStream, RecvStream),
        facts: (Vec<u8>, [u8; EXPORTER_LEN]),
    ) -> PairingChannel {
        let (send, recv) = stream;
        let (peer_spki, exporter) = facts;
        PairingChannel {
            conn,
            _endpoint: endpoint,
            send,
            recv,
            decoder: FrameDecoder::new(MAX_PAYLOAD),
            peer_spki,
            exporter,
            recv_timeout: RECV_TIMEOUT,
        }
    }

    /// The SPKI the peer presented in the TLS handshake (91-byte P-256 SPKI). The peer proved it
    /// holds the matching private key, nothing more: it is not trusted until the SAS is confirmed.
    pub fn peer_spki(&self) -> &[u8] {
        &self.peer_spki
    }

    /// `EXPORTER_LEN` bytes of TLS keying material with `EXPORTER_LABEL` and an empty context.
    /// Identical on both ends of one connection, different on any other connection.
    pub fn exporter(&self) -> [u8; EXPORTER_LEN] {
        self.exporter
    }

    /// Send one message.
    ///
    /// # Panics
    /// Panics if `msg` was built by hand with invalid fields (`PairingMsg::encode`'s contract);
    /// messages from the state machines and decoded ones are always valid.
    pub async fn send(&mut self, msg: &PairingMsg) -> Result<(), TransportError> {
        self.write_frame(&msg.encode()).await
    }

    /// The next message. Times out after 120 s (the pairing window's length) with
    /// [`TransportError::Timeout`]; the connection stays open.
    ///
    /// A malformed frame, an oversized payload, any kind but `KIND_PAIRING` or a payload that does
    /// not decode closes the connection (application code 1) and returns an error.
    pub async fn recv(&mut self) -> Result<PairingMsg, TransportError> {
        let frame = match timeout(self.recv_timeout, self.read_frame()).await {
            Ok(frame) => frame?,
            Err(_) => return Err(TransportError::Timeout),
        };
        PairingMsg::decode(&frame.payload).map_err(|error| self.protocol_error(&error.to_string()))
    }

    /// Close the connection with `reason`, after giving what was already sent a moment to arrive
    /// (the last message of an exchange must not be lost to the close).
    pub async fn close(mut self, reason: &str) {
        let _ = self.send.finish();
        let _ = timeout(CLOSE_FLUSH, self.send.stopped()).await;
        // The reason is a free-form phrase for the peer's logs; cap its size.
        let reason = reason.as_bytes();
        let reason = reason.get(..MAX_REASON).unwrap_or(reason);
        self.conn.close(VarInt::from_u32(CODE_NORMAL), reason);
    }

    async fn write_frame(&mut self, payload: &[u8]) -> Result<(), TransportError> {
        let frame = encode_frame(payload)?;
        self.send.write_all(&frame).await.map_err(write_error)
    }

    /// The next frame, which is a well-formed `KIND_PAIRING` frame. Cancel-safe: the partial frame
    /// stays in the decoder.
    async fn read_frame(&mut self) -> Result<Frame, TransportError> {
        let mut chunk = [0; READ_CHUNK];
        loop {
            match self.decoder.next_frame() {
                Ok(Some(frame)) if frame.kind == KIND_PAIRING => return Ok(frame),
                Ok(Some(_)) => return Err(self.protocol_error("unexpected frame kind")),
                Ok(None) => {}
                Err(error) => return Err(self.protocol_error(&error.to_string())),
            }
            match self.recv.read(&mut chunk).await {
                Ok(Some(read)) => self.decoder.push(&chunk[..read]),
                Ok(None) => {
                    return Err(TransportError::Connect(
                        "the peer ended the pairing stream".into(),
                    ));
                }
                Err(error) => return Err(read_error(error)),
            }
        }
    }

    /// The peer broke the pairing protocol: close with code 1. `why` describes the fault and never
    /// carries message contents.
    fn protocol_error(&self, why: &str) -> TransportError {
        self.conn
            .close(VarInt::from_u32(CODE_PROTOCOL_ERROR), b"protocol error");
        TransportError::Connect(format!("pairing protocol error: {why}"))
    }
}

// ---- the listener -------------------------------------------------------------------------------

/// Accepts pairing connections while it exists. The agent creates it when the user opens a pairing
/// window and drops it when the window closes.
///
/// Dropping it stops new connections at once (the endpoint refuses them) and abandons handshakes
/// in progress and channels nobody accepted. A [`PairingChannel`] already handed out keeps
/// working: it has its own 120 s limits.
pub struct PairingListener {
    endpoint: Endpoint,
    local_addr: SocketAddr,
    ready: Mutex<mpsc::Receiver<PairingChannel>>,
    accept_task: JoinHandle<()>,
}

impl fmt::Debug for PairingListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingListener")
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

impl Drop for PairingListener {
    fn drop(&mut self) {
        // Stop the loop (it holds an endpoint handle) and make the endpoint refuse new connections.
        // The endpoint itself lives on while a handed-out channel uses it.
        self.accept_task.abort();
        self.endpoint.set_server_config(None);
    }
}

impl PairingListener {
    /// Bind (inside a tokio runtime). Presents `identity`'s raw public key.
    pub fn bind(
        addr: SocketAddr,
        identity: Arc<DeviceIdentity>,
    ) -> Result<PairingListener, TransportError> {
        let crypto = QuicServerConfig::try_from(server_tls(&identity)?).map_err(tls_error)?;
        let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        config.transport_config(transport_config(1));

        // Fails with an I/O error if there is no tokio runtime.
        let endpoint = Endpoint::server(config, addr).map_err(TransportError::Bind)?;
        let local_addr = endpoint.local_addr().map_err(TransportError::Bind)?;
        let (ready_tx, ready_rx) = mpsc::channel(MAX_PENDING);
        let accept_task = tokio::spawn(accept_loop(endpoint.clone(), ready_tx));
        Ok(PairingListener {
            endpoint,
            local_addr,
            ready: Mutex::new(ready_rx),
            accept_task,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The next completed pairing connection: TLS is done and the dialer's stream is open. Its peer
    /// is unauthenticated: only the SAS exchange authenticates it.
    pub async fn accept(&self) -> Result<PairingChannel, TransportError> {
        let mut ready = self.ready.lock().await;
        loop {
            let channel = ready.recv().await.ok_or_else(|| {
                TransportError::Connect("the pairing listener has stopped".into())
            })?;
            // One that waited too long may be gone already.
            if channel.conn.close_reason().is_none() {
                return Ok(channel);
            }
        }
    }
}

/// Take incoming connections until the endpoint stops, each through its own handshake task.
async fn accept_loop(endpoint: Endpoint, ready: mpsc::Sender<PairingChannel>) {
    let handshakes = Arc::new(AtomicUsize::new(0));
    while let Some(incoming) = endpoint.accept().await {
        if !incoming.remote_address_validated() {
            // Make the sender prove it can receive at its claimed address before any handshake
            // state exists for it. Clients come back on their own.
            if incoming.retry().is_err() {
                tracing::debug!("could not send a retry");
            }
            continue;
        }
        let Some(slot) = HandshakeSlot::acquire(&handshakes, MAX_HANDSHAKES) else {
            // QUIC cannot carry an application code before the handshake is accepted, and
            // accepting is exactly the work being limited: refuse outright.
            tracing::debug!("too many pairing handshakes in flight; refusing one");
            incoming.refuse();
            continue;
        };
        let ready = ready.clone();
        let endpoint = endpoint.clone();
        tokio::spawn(async move {
            let established = async {
                let conn = incoming.await.map_err(connect_error)?;
                establish(conn, endpoint).await
            };
            let outcome = tokio::select! {
                // The listener was dropped: abandon the handshake.
                () = ready.closed() => return,
                outcome = timeout(HANDSHAKE_TIMEOUT, established) => outcome,
            };
            // The handshake is over, however it ended.
            drop(slot);
            match outcome {
                Ok(Ok(channel)) => match ready.try_send(channel) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(channel)) => {
                        tracing::debug!("pairing connection arrived while others were waiting");
                        channel
                            .conn
                            .close(VarInt::from_u32(CODE_BUSY), b"too many pairing connections");
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {}
                },
                Ok(Err(error)) => tracing::debug!(%error, "incoming pairing handshake failed"),
                Err(_) => tracing::debug!("incoming pairing handshake timed out"),
            }
        });
    }
}

/// Finish the server side of a connection whose TLS handshake is complete: take the dialer's
/// stream, whose first frame is the stream-open marker.
async fn establish(
    conn: quinn::Connection,
    endpoint: Endpoint,
) -> Result<PairingChannel, TransportError> {
    let facts = session_facts(&conn)?;
    let stream = conn.accept_bi().await.map_err(connect_error)?;
    let mut channel = PairingChannel::new(conn, endpoint, stream, facts);
    let marker = channel.read_frame().await?;
    if !marker.payload.is_empty() {
        return Err(channel.protocol_error("the stream did not open with the marker"));
    }
    Ok(channel)
}

// ---- the dialer ---------------------------------------------------------------------------------

/// Dial a pairing listener. Gives up after 5 s with [`TransportError::Timeout`].
pub async fn pair_connect(
    addr: SocketAddr,
    identity: Arc<DeviceIdentity>,
) -> Result<PairingChannel, TransportError> {
    let crypto = QuicClientConfig::try_from(client_tls(&identity)?).map_err(tls_error)?;
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    config.transport_config(transport_config(0));

    let local = match addr {
        SocketAddr::V4(_) => SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
        SocketAddr::V6(_) => SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)),
    };
    let endpoint = Endpoint::client(local).map_err(TransportError::Bind)?;
    match timeout(CONNECT_TIMEOUT, dial(&endpoint, config, addr)).await {
        Ok(channel) => channel,
        Err(_) => Err(TransportError::Timeout),
    }
}

async fn dial(
    endpoint: &Endpoint,
    config: quinn::ClientConfig,
    addr: SocketAddr,
) -> Result<PairingChannel, TransportError> {
    // Raw public keys carry no name and the verifier ignores it.
    let name = addr.ip().to_string();
    let connecting = endpoint
        .connect_with(config, addr, &name)
        .map_err(|error| TransportError::Connect(error.to_string()))?;
    let conn = connecting.await.map_err(connect_error)?;
    let facts = session_facts(&conn)?;
    let stream = conn.open_bi().await.map_err(connect_error)?;
    let mut channel = PairingChannel::new(conn, endpoint.clone(), stream, facts);
    // The listener cannot see the stream until something is sent on it (see the module docs).
    channel.write_frame(&[]).await?;
    Ok(channel)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::net::Ipv4Addr;

    use rustls::{ClientConnection, HandshakeKind, ServerConnection};

    use super::*;

    fn identity() -> Arc<DeviceIdentity> {
        Arc::new(DeviceIdentity::generate().unwrap())
    }

    fn loopback() -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
    }

    fn verifier() -> PairingVerifier {
        PairingVerifier::new(&aws_lc_rs::default_provider())
    }

    #[test]
    fn any_well_formed_spki_passes_and_malformed_ones_do_not() {
        let stranger = DeviceIdentity::generate().unwrap();
        let good = CertificateDer::from(stranger.spki().to_vec());
        assert!(PairingVerifier::check_key(&good, &[]).is_ok());
        // Anything after the end-entity key is not a raw public key exchange.
        assert!(PairingVerifier::check_key(&good, std::slice::from_ref(&good)).is_err());

        let mut off_curve = stranger.spki().to_vec();
        *off_curve.last_mut().unwrap() ^= 0x01;
        let mut wrong_prefix = stranger.spki().to_vec();
        wrong_prefix[5] ^= 0xff;
        let bad: [Vec<u8>; 5] = [
            Vec::new(),
            vec![0x30; 91],
            stranger.spki()[..90].to_vec(),
            off_curve,
            wrong_prefix,
        ];
        for spki in bad {
            let spki = CertificateDer::from(spki);
            assert!(PairingVerifier::check_key(&spki, &[]).is_err());
        }
    }

    #[test]
    fn both_verifiers_require_raw_keys_mutual_authentication_and_p256_only() {
        let verifier = verifier();
        assert!(ServerCertVerifier::requires_raw_public_keys(&verifier));
        assert!(ClientCertVerifier::requires_raw_public_keys(&verifier));
        assert!(ClientCertVerifier::offer_client_auth(&verifier));
        assert!(ClientCertVerifier::client_auth_mandatory(&verifier));
        assert_eq!(
            ServerCertVerifier::supported_verify_schemes(&verifier),
            [SCHEME]
        );
        assert_eq!(
            ClientCertVerifier::supported_verify_schemes(&verifier),
            [SCHEME]
        );
    }

    /// Drive a handshake between two in-memory connections; true if either side failed.
    fn handshake(client: &mut ClientConnection, server: &mut ServerConnection) -> bool {
        let mut failed = false;
        for _ in 0..8 {
            let mut wire = Vec::new();
            while client.wants_write() {
                client.write_tls(&mut wire).unwrap();
            }
            let mut input = wire.as_slice();
            while !input.is_empty() {
                server.read_tls(&mut input).unwrap();
                failed |= server.process_new_packets().is_err();
            }
            let mut wire = Vec::new();
            while server.wants_write() {
                server.write_tls(&mut wire).unwrap();
            }
            let mut input = wire.as_slice();
            while !input.is_empty() {
                client.read_tls(&mut input).unwrap();
                failed |= client.process_new_packets().is_err();
            }
        }
        failed
    }

    fn connections(
        client: rustls::ClientConfig,
        server: rustls::ServerConfig,
    ) -> (ClientConnection, ServerConnection) {
        (
            ClientConnection::new(Arc::new(client), ServerName::try_from("crosspane").unwrap())
                .unwrap(),
            ServerConnection::new(Arc::new(server)).unwrap(),
        )
    }

    /// Two strangers handshake, and every connection is a full handshake: nothing is resumable.
    #[test]
    fn strangers_handshake_with_the_pairing_alpn_and_never_resume() {
        let a = DeviceIdentity::generate().unwrap();
        let b = DeviceIdentity::generate().unwrap();
        let client_config = Arc::new(client_tls(&a).unwrap());
        let server_config = Arc::new(server_tls(&b).unwrap());
        for round in 0..3 {
            let mut client = ClientConnection::new(
                client_config.clone(),
                ServerName::try_from("crosspane").unwrap(),
            )
            .unwrap();
            let mut server = ServerConnection::new(server_config.clone()).unwrap();
            assert!(!handshake(&mut client, &mut server), "round {round}");
            assert!(!client.is_handshaking() && !server.is_handshaking());
            assert_eq!(client.handshake_kind(), Some(HandshakeKind::Full));
            assert_eq!(server.handshake_kind(), Some(HandshakeKind::Full));
            assert_eq!(client.alpn_protocol(), Some(PAIRING_ALPN));
            assert_eq!(server.alpn_protocol(), Some(PAIRING_ALPN));
            let seen = |keys: Option<&[CertificateDer<'_>]>| keys.map(|keys| keys[0].to_vec());
            assert_eq!(seen(server.peer_certificates()), Some(a.spki().to_vec()));
            assert_eq!(seen(client.peer_certificates()), Some(b.spki().to_vec()));
        }
    }

    /// Presenting someone else's key without holding it fails the handshake: the signature is
    /// checked even though no key is pinned.
    #[test]
    fn a_peer_that_does_not_hold_the_key_it_presents_fails_the_handshake() {
        let provider = Arc::new(aws_lc_rs::default_provider());
        let victim = DeviceIdentity::generate().unwrap();
        let attacker = DeviceIdentity::generate().unwrap();
        let honest = DeviceIdentity::generate().unwrap();
        // The victim's SPKI as the certificate, the attacker's key as the signer.
        let forged = Arc::new(CertifiedKey::new(
            vec![CertificateDer::from(victim.spki().to_vec())],
            load_certified_key(&provider, &attacker)
                .unwrap()
                .key
                .clone(),
        ));

        // A forging server fails the client's check.
        let (mut client, mut server) = connections(
            client_tls(&honest).unwrap(),
            server_tls_with(provider.clone(), forged.clone()).unwrap(),
        );
        assert!(handshake(&mut client, &mut server));
        // A forging client fails the server's check.
        let (mut client, mut server) = connections(
            client_tls_with(provider, forged).unwrap(),
            server_tls(&honest).unwrap(),
        );
        assert!(handshake(&mut client, &mut server));
        // Control: the same configurations with honest keys complete.
        let (mut client, mut server) =
            connections(client_tls(&victim).unwrap(), server_tls(&honest).unwrap());
        assert!(!handshake(&mut client, &mut server));
    }

    #[test]
    fn frames_have_the_wire_header_and_respect_the_payload_cap() {
        let frame = encode_frame(&[7, 8, 9]).unwrap();
        assert_eq!(
            frame,
            [WIRE_VERSION, KIND_PAIRING, 0, 0, 3, 0, 0, 0, 7, 8, 9]
        );
        assert_eq!(
            encode_frame(&[]).unwrap(),
            [WIRE_VERSION, KIND_PAIRING, 0, 0, 0, 0, 0, 0]
        );
        assert!(encode_frame(&[0; MAX_PAYLOAD]).is_ok());
        assert!(encode_frame(&[0; MAX_PAYLOAD + 1]).is_err());
    }

    #[tokio::test]
    async fn recv_times_out_and_leaves_the_connection_open() {
        let listener = PairingListener::bind(loopback(), identity()).unwrap();
        let (accepted, dialed) = tokio::join!(
            listener.accept(),
            pair_connect(listener.local_addr(), identity())
        );
        let (mut accepted, mut dialed) = (accepted.unwrap(), dialed.unwrap());
        accepted.recv_timeout = Duration::from_millis(150);
        assert!(matches!(
            accepted.recv().await,
            Err(TransportError::Timeout)
        ));
        // Still usable afterwards.
        dialed.send(&PairingMsg::Matched).await.unwrap();
        accepted.recv_timeout = Duration::from_secs(5);
        assert_eq!(accepted.recv().await.unwrap(), PairingMsg::Matched);
    }

    #[test]
    fn debug_output_names_no_key_material() {
        // Compile-time check that both types are Debug; the impls print only addresses.
        fn is_debug<T: fmt::Debug>() {}
        is_debug::<PairingListener>();
        is_debug::<PairingChannel>();
    }
}
