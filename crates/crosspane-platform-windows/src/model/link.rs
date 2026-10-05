//! Pure adapter classification, fact normalization and notification coalescing.
use crosspane_platform::{Interface, LinkClass};
use std::net::IpAddr;

pub struct Classification<'a> {
    pub if_type: u32,
    pub physical_medium: Option<u32>,
    pub tunnel_type: u32,
    pub virtual_hint: Option<bool>,
    pub description: &'a str,
    pub friendly_name: &'a str,
}
impl std::fmt::Debug for Classification<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Classification").finish_non_exhaustive()
    }
}
pub fn classify(facts: &Classification<'_>) -> LinkClass {
    if facts.if_type == 71 {
        return LinkClass::Wifi;
    }
    if facts.if_type != 6 || facts.tunnel_type != 0 {
        return LinkClass::Unknown;
    }
    let description = facts.description.to_ascii_lowercase();
    let friendly = facts.friendly_name.to_ascii_lowercase();
    if [
        "hyper-v",
        "vethernet",
        "vpn",
        "tap-",
        "tap ",
        "tun ",
        "wireguard",
        "loopback",
        "vmware",
        "virtualbox",
        "virtual ethernet",
    ]
    .iter()
    .any(|hint| description.contains(hint) || friendly.contains(hint))
    {
        return LinkClass::Unknown;
    }
    // [U] Description matching identifies current driver names, not authenticated hardware.
    // Microsoft documents USB4 host-to-host Ethernet and its adapter; driver/localization
    // variants remain unproven. https://learn.microsoft.com/windows-hardware/design/component-guidelines/usb4-interdomain-connections
    // Intel's driver name: https://www.thunderbolttechnology.net/sites/default/files/Thunderbolt%E2%84%A2%20Networking%20Bridging%20and%20Routing%20Instructional%20White%20Paper.pdf
    if ["usb4(tm) p2p network adapter", "thunderbolt(tm) networking"]
        .iter()
        .any(|name| {
            description == *name
                || description.strip_prefix(name).is_some_and(|suffix| {
                    suffix
                        .strip_prefix(" #")
                        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                })
        })
    {
        return LinkClass::DirectUsb4Tb;
    }
    if facts.virtual_hint != Some(false) {
        return LinkClass::Unknown;
    }
    match facts.physical_medium {
        Some(1 | 9) => LinkClass::Wifi,
        None | Some(0 | 14) => LinkClass::Lan,
        _ => LinkClass::Unknown,
    }
}

/// Scope IDs remain input facts for mismatch accounting. The frozen output contains
/// IP bytes only; IPv6 zones are derived from the interface's preferred IPv6 index.
pub struct Adapter {
    pub name: String,
    pub description: String,
    pub index_v4: u32,
    pub index_v6: u32,
    pub if_type: u32,
    pub physical_medium: Option<u32>,
    pub tunnel_type: u32,
    pub virtual_hint: Option<bool>,
    pub oper_status: u32,
    pub mtu: u32,
    pub receive_bps: u64,
    pub transmit_bps: u64,
    pub addrs: Vec<(IpAddr, u32)>,
}
impl std::fmt::Debug for Adapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Adapter").finish_non_exhaustive()
    }
}
pub struct Snapshot {
    pub interfaces: Vec<Interface>,
    pub scope_mismatches: usize,
}
impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field("count", &self.interfaces.len())
            .field("scope_mismatches", &self.scope_mismatches)
            .finish()
    }
}
pub fn normalize(adapters: Vec<Adapter>) -> Snapshot {
    let mut scope_mismatches = 0;
    let mut interfaces: Vec<_> = adapters
        .into_iter()
        .map(|adapter| {
            let index = if adapter.index_v6 != 0 {
                adapter.index_v6
            } else {
                adapter.index_v4
            };
            scope_mismatches += adapter
                .addrs
                .iter()
                .filter(|(ip, scope)| ip.is_ipv6() && *scope != index)
                .count();
            let class = classify(&Classification {
                if_type: adapter.if_type,
                physical_medium: adapter.physical_medium,
                tunnel_type: adapter.tunnel_type,
                virtual_hint: adapter.virtual_hint,
                description: &adapter.description,
                friendly_name: &adapter.name,
            });
            let mut addrs: Vec<_> = adapter.addrs.into_iter().map(|(ip, _)| ip).collect();
            addrs.sort_unstable();
            addrs.dedup();
            let speed_mbps = [adapter.receive_bps, adapter.transmit_bps]
                .into_iter()
                .filter(|speed| *speed != 0 && *speed != u64::MAX)
                .min()
                .map(|speed| speed / 1_000_000);
            Interface {
                name: adapter.name,
                index,
                class,
                up: adapter.oper_status == 1,
                mtu: Some(adapter.mtu).filter(|mtu| *mtu != 0 && *mtu != u32::MAX),
                speed_mbps,
                addrs,
            }
        })
        .collect();
    interfaces.sort_by(|a, b| a.index.cmp(&b.index).then_with(|| a.name.cmp(&b.name)));
    Snapshot {
        interfaces,
        scope_mismatches,
    }
}

/// Times are worker-local elapsed milliseconds; initial delivery never waits for debounce.
#[derive(Default)]
pub struct Coalescer {
    due: Option<u64>,
    last: Option<Vec<Interface>>,
}
impl std::fmt::Debug for Coalescer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Coalescer")
            .field("due", &self.due)
            .finish_non_exhaustive()
    }
}
impl Coalescer {
    pub fn signal(&mut self, now: u64) {
        self.due = Some(now.saturating_add(250));
    }
    pub fn due(&self, now: u64) -> bool {
        self.due.is_some_and(|due| now >= due)
    }
    pub fn initial(&mut self, snapshot: Vec<Interface>) -> Vec<Interface> {
        self.last = Some(snapshot.clone());
        snapshot
    }
    pub fn observed(&mut self, snapshot: Vec<Interface>) -> Option<Vec<Interface>> {
        self.due = None;
        if self.last.as_ref() == Some(&snapshot) {
            return None;
        }
        self.last = Some(snapshot.clone());
        Some(snapshot)
    }
}
