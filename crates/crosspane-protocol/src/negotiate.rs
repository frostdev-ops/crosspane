//! Version and feature negotiation (04 §3). Implemented in WP-1.2.

use crate::msg::Hello;

/// What both sides of a connection agreed to use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Negotiated {
    /// `min(local, remote)` minor version.
    pub minor: u32,
    /// Features both sides listed, sorted and deduplicated.
    pub features: Vec<String>,
}

/// Combine the local minor version and features with the peer's `Hello`.
pub fn negotiate(local_minor: u32, local_features: &[String], remote: &Hello) -> Negotiated {
    let _ = (local_minor, local_features, remote);
    Negotiated {
        minor: 0,
        features: Vec::new(),
    }
}
