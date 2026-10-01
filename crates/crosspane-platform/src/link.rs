//! Network interfaces and their link class (03 §2).

use std::net::IpAddr;
use std::sync::Arc;

use crate::{EventSink, PlatformError};

/// What kind of link an interface is, as classified from OS interface metadata (03 §2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LinkClass {
    /// USB4 or Thunderbolt host-to-host networking.
    DirectUsb4Tb,
    /// A point-to-point Ethernet cable.
    DirectEthernet,
    Lan,
    Wifi,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    /// The OS interface index (for IPv6 link-local scope IDs).
    pub index: u32,
    pub class: LinkClass,
    pub up: bool,
    pub mtu: Option<u32>,
    /// Negotiated link speed, if the OS reports it.
    pub speed_mbps: Option<u64>,
    pub addrs: Vec<IpAddr>,
}

/// Enumerates and classifies network interfaces.
pub trait LinkInfo: Send {
    /// A snapshot of every interface.
    fn interfaces(&self) -> Result<Vec<Interface>, PlatformError>;

    /// Start delivering full snapshots: the current one first, then one after every change.
    fn subscribe(&mut self, sink: Arc<dyn EventSink<Vec<Interface>>>) -> Result<(), PlatformError>;
}
