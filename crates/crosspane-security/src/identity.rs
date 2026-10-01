//! P-256 device identities and the canonical public-key format (04 §2).

use core::fmt;

use aws_lc_rs::digest::{SHA256, digest};
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{
    ECDSA_P256_SHA256_ASN1, ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair,
    UnparsedPublicKey,
};
use crosspane_types::id::NodeId;
use zeroize::Zeroizing;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    #[error("invalid private key")]
    InvalidKey,
    #[error("invalid public key")]
    InvalidSpki,
    #[error("key generation failed")]
    Generation,
}

pub const SPKI_LEN: usize = 91;

const SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];
const POINT_LEN: usize = 65;

/// This node's long-term identity (04 §2).
pub struct DeviceIdentity {
    keypair: EcdsaKeyPair,
    pkcs8: Zeroizing<Vec<u8>>,
    spki: Vec<u8>,
    node: NodeId,
}

impl DeviceIdentity {
    /// A new P-256 key pair from the system RNG.
    pub fn generate() -> Result<DeviceIdentity, IdentityError> {
        // The provider's temporary PKCS#8 Document also zeroizes itself on drop.
        let document =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &SystemRandom::new())
                .map_err(|_| IdentityError::Generation)?;
        Self::from_pkcs8(document.as_ref())
    }

    /// Load from PKCS#8 (as stored in the OS key store by a platform `KeyStore`).
    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<DeviceIdentity, IdentityError> {
        let pkcs8 = Zeroizing::new(pkcs8.to_vec());
        let keypair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &pkcs8)
            .map_err(|_| IdentityError::InvalidKey)?;
        let spki = spki_from_point(keypair.public_key().as_ref())?;
        let node = node_id(&spki);
        Ok(Self {
            keypair,
            pkcs8,
            spki,
            node,
        })
    }

    /// The PKCS#8 bytes to store.
    pub fn pkcs8(&self) -> &Zeroizing<Vec<u8>> {
        &self.pkcs8
    }

    pub fn spki(&self) -> &[u8] {
        &self.spki
    }

    pub fn node(&self) -> NodeId {
        self.node
    }

    /// ECDSA P-256/SHA-256 ASN.1 signature over `msg`.
    pub fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, IdentityError> {
        self.keypair
            .sign(&SystemRandom::new(), msg)
            .map(|signature| signature.as_ref().to_vec())
            .map_err(|_| IdentityError::InvalidKey)
    }
}

impl fmt::Debug for DeviceIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceIdentity({})", self.node.short())
    }
}

pub fn node_id(spki: &[u8]) -> NodeId {
    let hash = digest(&SHA256, spki);
    let mut bytes = [0; 32];
    bytes.copy_from_slice(hash.as_ref());
    NodeId(bytes)
}

pub fn spki_from_point(point: &[u8]) -> Result<Vec<u8>, IdentityError> {
    validate_point(point)?;
    let mut spki = Vec::with_capacity(SPKI_LEN);
    spki.extend_from_slice(&SPKI_PREFIX);
    spki.extend_from_slice(point);
    Ok(spki)
}

pub fn point_from_spki(spki: &[u8]) -> Result<&[u8], IdentityError> {
    if spki.len() != SPKI_LEN || !spki.starts_with(&SPKI_PREFIX) {
        return Err(IdentityError::InvalidSpki);
    }
    let point = &spki[SPKI_PREFIX.len()..];
    validate_point(point)?;
    Ok(point)
}

/// True only for a valid signature by the key in `spki`.
pub fn verify(spki: &[u8], msg: &[u8], signature: &[u8]) -> bool {
    let Ok(point) = point_from_spki(spki) else {
        return false;
    };
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, point)
        .verify(msg, signature)
        .is_ok()
}

fn validate_point(point: &[u8]) -> Result<(), IdentityError> {
    if point.len() != POINT_LEN || point.first() != Some(&0x04) {
        return Err(IdentityError::InvalidSpki);
    }
    // Parsing checks curve membership and rejects the point at infinity, without
    // treating a failed signature verification as public-key validation.
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, point)
        .parse()
        .map(|_| ())
        .map_err(|_| IdentityError::InvalidSpki)
}
