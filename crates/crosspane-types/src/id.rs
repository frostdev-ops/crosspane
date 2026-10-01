//! Identifiers shared by every crate.

use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A node's identity: SHA-256 of its device public key (SubjectPublicKeyInfo DER), see 04 §2.
///
/// Displayed and serialised as 64 lowercase hex characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub [u8; 32]);

impl NodeId {
    /// Short form for logs and UI: the first 8 bytes in lowercase hex.
    pub fn short(&self) -> String {
        hex(&self.0[..8])
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex(&self.0))
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", self.short())
    }
}

/// Error returned when a string is not 64 hex characters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseNodeIdError;

impl fmt::Display for ParseNodeIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a node ID is 64 hexadecimal characters")
    }
}

impl std::error::Error for ParseNodeIdError {}

impl FromStr for NodeId {
    type Err = ParseNodeIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = s.as_bytes();
        if bytes.len() != 64 {
            return Err(ParseNodeIdError);
        }
        let mut out = [0u8; 32];
        for (i, [hi, lo]) in bytes.as_chunks::<2>().0.iter().enumerate() {
            let hi = hex_value(*hi).ok_or(ParseNodeIdError)?;
            let lo = hex_value(*lo).ok_or(ParseNodeIdError)?;
            out[i] = (hi << 4) | lo;
        }
        Ok(NodeId(out))
    }
}

impl Serialize for NodeId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for NodeId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    s
}

fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// A display, local to one node. Stable while the display stays connected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DisplayId(pub u32);

/// A display anywhere in the workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GlobalDisplayId {
    pub node: NodeId,
    pub display: DisplayId,
}

/// A window on its source node: the platform's handle (Hyprland window address, macOS
/// `CGWindowID`, Windows `HWND`), widened to 64 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WindowId(pub u64);

/// One projection (01 §Vocabulary), unique on its source node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProjectionId(pub u64);

/// One E1 control session, unique on its controller node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionId(pub u64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_hex_round_trip() {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = u8::try_from(i * 7).unwrap_or(0);
        }
        let id = NodeId(bytes);
        let text = id.to_string();
        assert_eq!(text.len(), 64);
        assert_eq!(text.parse::<NodeId>(), Ok(id));
        assert_eq!(id.short(), text[..16]);
    }

    #[test]
    fn node_id_rejects_bad_input() {
        assert_eq!("abc".parse::<NodeId>(), Err(ParseNodeIdError));
        assert_eq!("g".repeat(64).parse::<NodeId>(), Err(ParseNodeIdError));
    }
}
