//! TLS 1.3 with RFC 7250 raw public keys, pinned through [`PinStore`] (04 §3).
//!
//! Both directions authenticate: the server requires a client key, and each side accepts the
//! other's SPKI only if the pin store knows it. There is no certificate chain and no web PKI.

use std::fmt;
use std::sync::Arc;

use crosspane_security::identity::DeviceIdentity;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
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

use crate::{PinStore, TransportError};

/// The only signature scheme Crosspane device keys use (P-256, 04 §2).
const SCHEME: SignatureScheme = SignatureScheme::ECDSA_NISTP256_SHA256;

/// Accepts a peer's raw public key only if the [`PinStore`] has it pinned, and checks the
/// handshake signature (proof of possession) with rustls's raw-key verification.
///
/// One instance serves both roles: it is the client's [`ServerCertVerifier`] and the server's
/// [`ClientCertVerifier`].
pub(crate) struct PinVerifier {
    pins: Arc<dyn PinStore>,
    algorithms: WebPkiSupportedAlgorithms,
}

// Do not let diagnostics dump pin store contents.
impl fmt::Debug for PinVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PinVerifier { .. }")
    }
}

impl PinVerifier {
    fn new(pins: Arc<dyn PinStore>, provider: &CryptoProvider) -> Self {
        Self {
            pins,
            algorithms: provider.signature_verification_algorithms,
        }
    }

    fn check_pin(
        &self,
        spki: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
    ) -> Result<(), Error> {
        // With raw public keys the "end entity certificate" is the bare SPKI; there is nothing
        // else in the chain.
        if !intermediates.is_empty() || self.pins.trusted(spki.as_ref()).is_none() {
            return Err(Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ));
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

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        self.check_pin(end_entity, intermediates)?;
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

impl ClientCertVerifier for PinVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    /// Mutual authentication is mandatory: a client without a pinned key is refused.
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
        self.check_pin(end_entity, intermediates)?;
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

/// The QUIC server and client crypto configurations for one node.
pub(crate) struct Configs {
    pub(crate) server: Arc<QuicServerConfig>,
    pub(crate) client: Arc<QuicClientConfig>,
}

impl fmt::Debug for Configs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Configs { .. }")
    }
}

/// Build both QUIC crypto configurations (see [`rustls_configs`]).
pub(crate) fn build(
    identity: &DeviceIdentity,
    pins: Arc<dyn PinStore>,
) -> Result<Configs, TransportError> {
    let (server, client) = rustls_configs(identity, pins)?;
    Ok(Configs {
        server: Arc::new(QuicServerConfig::try_from(server).map_err(tls_error)?),
        client: Arc::new(QuicClientConfig::try_from(client).map_err(tls_error)?),
    })
}

/// The TLS configurations: TLS 1.3 only, ALPN `crosspane/1`, raw public keys, mutual
/// authentication against `pins`, no 0-RTT and no session resumption.
fn rustls_configs(
    identity: &DeviceIdentity,
    pins: Arc<dyn PinStore>,
) -> Result<(rustls::ServerConfig, rustls::ClientConfig), TransportError> {
    // Select aws-lc-rs explicitly rather than relying on a process-wide default provider.
    let provider = Arc::new(aws_lc_rs::default_provider());
    let key = load_certified_key(&provider, identity)?;
    let verifier = Arc::new(PinVerifier::new(pins, &provider));

    let mut server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(tls_error)?
        .with_client_cert_verifier(verifier.clone())
        .with_cert_resolver(Arc::new(AlwaysResolvesServerRawPublicKeys::new(
            key.clone(),
        )));
    server.alpn_protocols = vec![crosspane_protocol::ALPN.to_vec()];
    // 0-RTT off: input messages must not be replayable (04 §3). A zero early-data limit makes the
    // server refuse early data; quinn requires exactly 0 or u32::MAX here.
    server.max_early_data_size = 0;
    // Resumption off: issue no tickets, keep no session storage, and use no ticketer. (The builder
    // default has no ticketer; the QUIC server must never hand out a resumable session.)
    server.send_tls13_tickets = 0;
    server.max_tls13_tickets = 0;
    server.session_storage = Arc::new(NoServerSessionStorage {});
    if server.ticketer.enabled() {
        return Err(TransportError::Tls("the server ticketer is enabled".into()));
    }

    let mut client = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(tls_error)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_cert_resolver(Arc::new(AlwaysResolvesClientRawPublicKeys::new(key)));
    client.alpn_protocols = vec![crosspane_protocol::ALPN.to_vec()];
    // 0-RTT off on the client too, and no stored sessions or tickets: every connection is a full
    // handshake.
    client.enable_early_data = false;
    client.resumption = Resumption::disabled();

    Ok((server, client))
}

/// The SPKI the peer authenticated with, from a completed handshake.
pub(crate) fn peer_spki(connection: &quinn::Connection) -> Option<Vec<u8>> {
    // quinn keeps the `CertificateDer` container type for raw public keys too: the single entry
    // is the peer's SPKI.
    let identity = connection.peer_identity()?;
    let keys = identity.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    match keys.as_slice() {
        [spki] => Some(spki.as_ref().to_vec()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;

    use rustls::{ClientConnection, HandshakeKind, ServerConnection};

    use super::*;
    use crosspane_types::id::NodeId;

    struct Pins(HashMap<Vec<u8>, NodeId>);

    impl PinStore for Pins {
        fn trusted(&self, spki: &[u8]) -> Option<NodeId> {
            self.0.get(spki).copied()
        }
    }

    fn pins_of(identities: &[&DeviceIdentity]) -> Arc<dyn PinStore> {
        Arc::new(Pins(
            identities
                .iter()
                .map(|identity| (identity.spki().to_vec(), identity.node()))
                .collect(),
        ))
    }

    fn verifier(pinned: &[&DeviceIdentity]) -> PinVerifier {
        PinVerifier::new(pins_of(pinned), &aws_lc_rs::default_provider())
    }

    #[test]
    fn only_pinned_keys_pass_and_chains_are_refused() {
        let pinned = DeviceIdentity::generate().unwrap();
        let stranger = DeviceIdentity::generate().unwrap();
        let verifier = verifier(&[&pinned]);
        let pinned_der = CertificateDer::from(pinned.spki().to_vec());
        let stranger_der = CertificateDer::from(stranger.spki().to_vec());

        assert!(verifier.check_pin(&pinned_der, &[]).is_ok());
        assert!(verifier.check_pin(&stranger_der, &[]).is_err());
        // Anything after the end-entity key is not a raw public key exchange.
        assert!(verifier.check_pin(&pinned_der, &[stranger_der]).is_err());
    }

    #[test]
    fn both_verifiers_require_raw_keys_and_mutual_authentication() {
        let verifier = verifier(&[]);
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

    /// Run a TLS handshake between two in-memory connections until neither has more to say.
    fn handshake(client: &mut ClientConnection, server: &mut ServerConnection) {
        loop {
            let mut progressed = false;
            let mut wire = Vec::new();
            while client.wants_write() {
                client.write_tls(&mut wire).unwrap();
                progressed = true;
            }
            let mut input = wire.as_slice();
            while !input.is_empty() {
                server.read_tls(&mut input).unwrap();
                server.process_new_packets().unwrap();
            }
            let mut wire = Vec::new();
            while server.wants_write() {
                server.write_tls(&mut wire).unwrap();
                progressed = true;
            }
            let mut input = wire.as_slice();
            while !input.is_empty() {
                client.read_tls(&mut input).unwrap();
                client.process_new_packets().unwrap();
            }
            if !progressed {
                return;
            }
        }
    }

    /// Mutual raw-public-key authentication works, and a second connection with the same configs
    /// is a full handshake again: nothing is resumable.
    #[test]
    fn handshakes_are_full_every_time() {
        let a = DeviceIdentity::generate().unwrap();
        let b = DeviceIdentity::generate().unwrap();
        let (_, client_config) = rustls_configs(&a, pins_of(&[&b])).unwrap();
        let (server_config, _) = rustls_configs(&b, pins_of(&[&a])).unwrap();
        let (client_config, server_config) = (Arc::new(client_config), Arc::new(server_config));

        for round in 0..3 {
            let mut client = ClientConnection::new(
                client_config.clone(),
                ServerName::try_from("crosspane").unwrap(),
            )
            .unwrap();
            let mut server = ServerConnection::new(server_config.clone()).unwrap();
            handshake(&mut client, &mut server);
            assert!(!client.is_handshaking() && !server.is_handshaking());
            assert_eq!(
                client.handshake_kind(),
                Some(HandshakeKind::Full),
                "round {round}"
            );
            assert_eq!(
                server.handshake_kind(),
                Some(HandshakeKind::Full),
                "round {round}"
            );
            assert_eq!(client.alpn_protocol(), Some(crosspane_protocol::ALPN));
            // The server saw exactly the client's pinned key, and vice versa.
            let seen = |keys: Option<&[CertificateDer<'_>]>| keys.map(|keys| keys[0].to_vec());
            assert_eq!(seen(server.peer_certificates()), Some(a.spki().to_vec()));
            assert_eq!(seen(client.peer_certificates()), Some(b.spki().to_vec()));
        }
    }

    #[test]
    fn a_server_that_pins_nobody_refuses_the_handshake() {
        let a = DeviceIdentity::generate().unwrap();
        let b = DeviceIdentity::generate().unwrap();
        let (_, client_config) = rustls_configs(&a, pins_of(&[&b])).unwrap();
        let (server_config, _) = rustls_configs(&b, pins_of(&[])).unwrap();
        let mut client = ClientConnection::new(
            Arc::new(client_config),
            ServerName::try_from("crosspane").unwrap(),
        )
        .unwrap();
        let mut server = ServerConnection::new(Arc::new(server_config)).unwrap();
        let mut wire = Vec::new();
        let mut failed = false;
        for _ in 0..8 {
            wire.clear();
            while client.wants_write() {
                client.write_tls(&mut wire).unwrap();
            }
            let mut input = wire.as_slice();
            while !input.is_empty() {
                server.read_tls(&mut input).unwrap();
                failed |= server.process_new_packets().is_err();
            }
            wire.clear();
            while server.wants_write() {
                server.write_tls(&mut wire).unwrap();
            }
            let mut input = wire.as_slice();
            while !input.is_empty() {
                client.read_tls(&mut input).unwrap();
                failed |= client.process_new_packets().is_err();
            }
        }
        assert!(failed, "an unpinned client key must fail the handshake");
    }
}
