//! mDNS/DNS-SD discovery of Crosspane nodes on the local network (03 §2 step 1, 04 §3,
//! WP-1.6).
//!
//! Each node advertises `_crosspane._udp.local.` with a **random instance id** (new at every
//! start) and its protocol version, and **no device name**, so names don't leak on shared Wi-Fi.
//! While a pairing window is open the advertisement also carries the device name, so the other
//! machine can offer it in a list. Paired peers are recognised by their keys in the TLS handshake
//! after dialling a candidate, never by anything in the advertisement.

use std::net::SocketAddr;

/// The DNS-SD service type.
pub const SERVICE_TYPE: &str = "_crosspane._udp.local.";

/// Another node seen on the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// Its random instance id (changes when it restarts).
    pub instance: String,
    /// Where it listens: every address it advertised, with its agent port.
    pub addrs: Vec<SocketAddr>,
    /// The protocol major version it advertises.
    pub version: u32,
    /// Its device name, only while it has a pairing window open.
    pub pairing_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoveryEvent {
    /// A node appeared, or its addresses or pairing name changed.
    Found(Candidate),
    /// A node went away (its advertisement was withdrawn or expired).
    Lost { instance: String },
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("mDNS failed: {0}")]
    Mdns(String),
}

/// Advertising and browsing, on a background thread. Dropping it withdraws the advertisement.
pub struct Discovery {
    _private: (),
}

impl std::fmt::Debug for Discovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Discovery").finish_non_exhaustive()
    }
}

impl Discovery {
    /// Advertise this node's agent `port` and browse for other nodes. `events` is called from a
    /// background thread for every change; it must not block. This node's own advertisement is
    /// never reported.
    ///
    /// The agent treats an `Err` as "no discovery" and runs on manual addresses.
    pub fn start(
        port: u16,
        events: Box<dyn Fn(DiscoveryEvent) + Send + Sync>,
    ) -> Result<Discovery, DiscoveryError> {
        // WP-1.6 implements this.
        let _ = (port, events);
        Err(DiscoveryError::Mdns("not implemented yet".into()))
    }

    /// While `Some(name)`, the advertisement carries the device name (pairing mode, 04 §3);
    /// `None` withdraws it again.
    pub fn set_pairing_name(&self, name: Option<&str>) -> Result<(), DiscoveryError> {
        let _ = name;
        Ok(())
    }

    /// The nodes currently seen, not including this one.
    pub fn candidates(&self) -> Vec<Candidate> {
        Vec::new()
    }
}
