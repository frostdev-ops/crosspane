//! Pinned peers, locally granted permissions, and signed revocation (04 §2–§4).

use std::collections::{BTreeMap, BTreeSet};

use aws_lc_rs::constant_time::verify_slices_are_equal;
use crosspane_protocol::msg::{Capability, RevocationNotice};
use crosspane_types::id::NodeId;
use serde::{Deserialize, Serialize};

use crate::identity::{DeviceIdentity, SPKI_LEN, node_id, point_from_spki, verify};

/// 04 §2 defaults when pairing: WindowShare and WindowPresent on; InputAccept and WindowBrowse off.
pub fn default_grants() -> BTreeSet<Capability> {
    BTreeSet::from([Capability::WindowShare, Capability::WindowPresent])
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerEntry {
    pub node: NodeId,
    /// Hex in serde.
    #[serde(with = "spki_hex")]
    pub spki: Vec<u8>,
    pub name: String,
    /// What THIS node grants the peer. Enforced here.
    #[serde(with = "grants_serde")]
    pub granted: BTreeSet<Capability>,
    pub paired_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TrustError {
    #[error("the node ID doesn't match the key")]
    NodeMismatch,
    #[error("invalid public key")]
    InvalidSpki,
    #[error("unknown peer")]
    UnknownPeer,
    #[error("the revocation's issuer isn't a trusted peer")]
    UntrustedIssuer,
    #[error("the revocation's signature is invalid")]
    BadSignature,
    #[error("the trust store file is invalid")]
    Corrupt,
}

/// What applying a revocation did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Revoked {
    /// The revoked peer was forgotten (it may also have been unknown here).
    Applied { forgotten: Option<PeerEntry> },
    /// The notice revokes this node itself: ignored (a malicious notice can only cause denial of
    /// service; 04 §4).
    IgnoredSelf,
    /// Already applied earlier.
    Duplicate,
    /// Issued before the revoked node's current pairing (it was paired again since): ignored, so
    /// an old notice can't undo a fresh pairing.
    Stale,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrustStore {
    peers: BTreeMap<NodeId, PeerEntry>,
    revoked: BTreeSet<NodeId>,
}

impl TrustStore {
    pub fn new() -> TrustStore {
        Self::default()
    }

    /// Pin a peer after pairing. Requires `node == node_id(spki)` and a valid SPKI. Replaces an
    /// existing entry, and removes the node from the revoked set: pinning is an explicit user
    /// action through a fresh pairing.
    pub fn pin(&mut self, entry: PeerEntry) -> Result<(), TrustError> {
        point_from_spki(&entry.spki).map_err(|_| TrustError::InvalidSpki)?;
        if verify_slices_are_equal(&entry.node.0, &node_id(&entry.spki).0).is_err() {
            return Err(TrustError::NodeMismatch);
        }
        self.revoked.remove(&entry.node);
        self.peers.insert(entry.node, entry);
        Ok(())
    }

    /// "Forget device" (04 §4). Returns the entry if it existed.
    pub fn forget(&mut self, node: NodeId) -> Option<PeerEntry> {
        self.peers.remove(&node)
    }

    /// Revoke a lost or stolen device from this node (04 §4): forget it and refuse it until a
    /// fresh pairing pins it again. The caller sends [`TrustStore::issue_revocation`]'s notice to
    /// the other peers. Returns the entry if it was pinned.
    pub fn revoke(&mut self, node: NodeId) -> Option<PeerEntry> {
        self.revoked.insert(node);
        self.peers.remove(&node)
    }

    pub fn get(&self, node: NodeId) -> Option<&PeerEntry> {
        self.peers.get(&node)
    }

    pub fn peers(&self) -> Vec<&PeerEntry> {
        self.peers.values().collect()
    }

    /// For the transport's verifier (04 §3): the peer's NodeId if this SPKI is pinned and not
    /// revoked. Compare in constant time.
    pub fn trusted(&self, spki: &[u8]) -> Option<NodeId> {
        if spki.len() != SPKI_LEN {
            return None;
        }
        let mut trusted = None;
        // Compare all pins using the audited primitive, without an early return
        // revealing the matching pin's position. Every pinned SPKI is valid.
        for peer in self.peers.values() {
            let matches = verify_slices_are_equal(&peer.spki, spki).is_ok();
            if matches && !self.is_revoked(peer.node) {
                trusted = Some(peer.node);
            }
        }
        trusted
    }

    /// Permission check, enforced by this (granting) node: false for unknown or revoked peers.
    pub fn allows(&self, node: NodeId, capability: Capability) -> bool {
        !self.is_revoked(node)
            && self
                .get(node)
                .is_some_and(|peer| peer.granted.contains(&capability))
    }

    pub fn set_grant(
        &mut self,
        node: NodeId,
        capability: Capability,
        granted: bool,
    ) -> Result<(), TrustError> {
        if self.is_revoked(node) {
            return Err(TrustError::UnknownPeer);
        }
        let peer = self.peers.get_mut(&node).ok_or(TrustError::UnknownPeer)?;
        if granted {
            peer.granted.insert(capability);
        } else {
            peer.granted.remove(&capability);
        }
        Ok(())
    }

    /// Create a signed notice revoking `revoked`, issued by `issuer` at `now_ms`.
    pub fn issue_revocation(
        issuer: &DeviceIdentity,
        revoked: NodeId,
        now_ms: u64,
    ) -> Result<RevocationNotice, TrustError> {
        let issuer_node = issuer.node();
        let signed = RevocationNotice::signed_bytes(&revoked, &issuer_node, now_ms);
        let signature = issuer.sign(&signed).map_err(|_| TrustError::BadSignature)?;
        Ok(RevocationNotice {
            revoked,
            issuer: issuer_node,
            issued_at_ms: now_ms,
            signature,
        })
    }

    /// Verify and apply a notice received from a peer. `own` is this node's ID.
    /// The issuer must be pinned and not revoked; self-revocations are ignored.
    pub fn apply_revocation(
        &mut self,
        notice: &RevocationNotice,
        own: NodeId,
    ) -> Result<Revoked, TrustError> {
        let issuer = self
            .get(notice.issuer)
            .filter(|_| !self.is_revoked(notice.issuer))
            .ok_or(TrustError::UntrustedIssuer)?;
        let signed =
            RevocationNotice::signed_bytes(&notice.revoked, &notice.issuer, notice.issued_at_ms);
        if !verify(&issuer.spki, &signed, &notice.signature) {
            return Err(TrustError::BadSignature);
        }
        // Authenticate even self-revocations and duplicates before returning.
        if notice.revoked == own {
            return Ok(Revoked::IgnoredSelf);
        }
        if self.is_revoked(notice.revoked) {
            return Ok(Revoked::Duplicate);
        }
        if self
            .get(notice.revoked)
            .is_some_and(|entry| entry.paired_at_ms > notice.issued_at_ms)
        {
            return Ok(Revoked::Stale);
        }
        let forgotten = self.forget(notice.revoked);
        self.revoked.insert(notice.revoked);
        Ok(Revoked::Applied { forgotten })
    }

    pub fn is_revoked(&self, node: NodeId) -> bool {
        self.revoked.contains(&node)
    }

    /// JSON for the agent to persist; round-trips exactly.
    pub fn to_json(&self) -> String {
        // Only strings, integers, and the frozen capability variants are
        // serialized; there are no fallible JSON map keys or numeric values.
        serde_json::json!({ "peers": self.peers(), "revoked": self.revoked }).to_string()
    }

    /// Validate every entry as `pin` does; any invalid store is `Corrupt`.
    pub fn from_json(json: &str) -> Result<TrustStore, TrustError> {
        let stored: StoredTrust = serde_json::from_str(json).map_err(|_| TrustError::Corrupt)?;
        let mut store = Self::new();
        for peer in stored.peers {
            // Neither duplicate pins nor pinned/revoked overlap can be produced
            // by the public API; reject them rather than silently repairing data.
            if store.get(peer.node).is_some() || stored.revoked.contains(&peer.node) {
                return Err(TrustError::Corrupt);
            }
            store.pin(peer).map_err(|_| TrustError::Corrupt)?;
        }
        store.revoked = stored.revoked;
        Ok(store)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredTrust {
    peers: Vec<PeerEntry>,
    revoked: BTreeSet<NodeId>,
}

mod spki_hex {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut hex = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            hex.push(char::from(DIGITS[usize::from(byte >> 4)]));
            hex.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
        }
        serializer.serialize_str(&hex)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let hex = String::deserialize(deserializer)?;
        let (pairs, remainder) = hex.as_bytes().as_chunks::<2>();
        if !remainder.is_empty() {
            return Err(serde::de::Error::custom("odd-length SPKI hex"));
        }
        pairs
            .iter()
            .map(|[hi, lo]| {
                let hi = digit(*hi).ok_or_else(|| serde::de::Error::custom("invalid SPKI hex"))?;
                let lo = digit(*lo).ok_or_else(|| serde::de::Error::custom("invalid SPKI hex"))?;
                Ok((hi << 4) | lo)
            })
            .collect()
    }

    fn digit(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
}

// Capability is a frozen protocol enum without serde implementations. The
// field codec keeps its public type and serializes the frozen variant names.
mod grants_serde {
    use std::collections::BTreeSet;

    use crosspane_protocol::msg::Capability;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        grants: &BTreeSet<Capability>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let names: Result<Vec<_>, S::Error> = grants
            .iter()
            .map(|capability| match capability {
                Capability::InputAccept => Ok("InputAccept"),
                Capability::WindowShare => Ok("WindowShare"),
                Capability::WindowBrowse => Ok("WindowBrowse"),
                Capability::WindowPresent => Ok("WindowPresent"),
                Capability::AudioSpeaker => Ok("AudioSpeaker"),
                Capability::AudioMic => Ok("AudioMic"),
                Capability::ClipboardRead => Ok("ClipboardRead"),
                Capability::ClipboardWrite => Ok("ClipboardWrite"),
                _ => Err(serde::ser::Error::custom("unknown capability")),
            })
            .collect();
        names?.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeSet<Capability>, D::Error> {
        let names = Vec::<String>::deserialize(deserializer)?;
        names
            .iter()
            .map(|name| match name.as_str() {
                "InputAccept" => Ok(Capability::InputAccept),
                "WindowShare" => Ok(Capability::WindowShare),
                "WindowBrowse" => Ok(Capability::WindowBrowse),
                "WindowPresent" => Ok(Capability::WindowPresent),
                "AudioSpeaker" => Ok(Capability::AudioSpeaker),
                "AudioMic" => Ok(Capability::AudioMic),
                "ClipboardRead" => Ok(Capability::ClipboardRead),
                "ClipboardWrite" => Ok(Capability::ClipboardWrite),
                _ => Err(serde::de::Error::custom("unknown capability")),
            })
            .collect()
    }
}
