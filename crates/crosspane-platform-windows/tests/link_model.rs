use crosspane_platform::{Interface, LinkClass};
use crosspane_platform_windows::model::link::{
    Adapter, Classification, Coalescer, classify, normalize,
};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

fn facts(if_type: u32) -> Classification<'static> {
    Classification {
        if_type,
        physical_medium: Some(14),
        tunnel_type: 0,
        virtual_hint: Some(false),
        description: "fixture Ethernet",
        friendly_name: "fixture",
    }
}
fn adapter() -> Adapter {
    Adapter {
        name: "fixture".into(),
        description: "fixture Ethernet".into(),
        index_v4: 7,
        index_v6: 9,
        if_type: 6,
        physical_medium: Some(14),
        tunnel_type: 0,
        virtual_hint: Some(false),
        oper_status: 1,
        mtu: 1500,
        receive_bps: 1_000_000_000,
        transmit_bps: 100_000_000,
        addrs: Vec::new(),
    }
}
fn snapshot(up: bool) -> Vec<Interface> {
    vec![Interface {
        name: "fixture".into(),
        index: 9,
        class: LinkClass::Lan,
        up,
        mtu: Some(1500),
        speed_mbps: Some(100),
        addrs: Vec::new(),
    }]
}

#[test]
fn physical_ethernet_is_lan_never_direct_ethernet() {
    assert_eq!(classify(&facts(6)), LinkClass::Lan);
    for if_type in 0..=255 {
        assert_ne!(classify(&facts(if_type)), LinkClass::DirectEthernet);
    }
}
#[test]
fn wifi_type_and_ambiguous_ethernet_physical_medium() {
    assert_eq!(classify(&facts(71)), LinkClass::Wifi);
    let mut input = facts(6);
    input.physical_medium = Some(9);
    assert_eq!(classify(&input), LinkClass::Wifi);
}
#[test]
fn loopback_tunnel_virtual_and_unproven_ethernet_stay_unknown() {
    for if_type in [1, 24, 131, 144] {
        assert_eq!(classify(&facts(if_type)), LinkClass::Unknown);
    }
    let mut input = facts(6);
    input.tunnel_type = 15;
    assert_eq!(classify(&input), LinkClass::Unknown);
    input.tunnel_type = 0;
    input.virtual_hint = Some(true);
    assert_eq!(classify(&input), LinkClass::Unknown);
    input.virtual_hint = None;
    assert_eq!(classify(&input), LinkClass::Unknown);
}
#[test]
fn known_virtual_names_do_not_become_lan() {
    for name in [
        "Hyper-V Virtual Ethernet Adapter",
        "vEthernet (fixture)",
        "TAP-Windows Adapter",
        "fixture VPN",
        "WireGuard",
        "VMware Virtual Ethernet Adapter",
        "Loopback",
    ] {
        let mut input = facts(6);
        input.description = name;
        assert_eq!(classify(&input), LinkClass::Unknown);
    }
}
#[test]
fn named_usb4_and_thunderbolt_are_explicit_heuristics() {
    for name in [
        "USB4(TM) P2P Network Adapter",
        "Thunderbolt(TM) Networking",
        "Thunderbolt(TM) Networking #2",
    ] {
        let mut input = facts(6);
        input.description = name;
        input.virtual_hint = Some(true);
        assert_eq!(classify(&input), LinkClass::DirectUsb4Tb);
    }
    let mut input = facts(131);
    input.description = "USB4(TM) P2P Network Adapter";
    assert_eq!(classify(&input), LinkClass::Unknown);
    input = facts(6);
    input.virtual_hint = None;
    input.friendly_name = "USB4 cable fixture";
    assert_eq!(classify(&input), LinkClass::Unknown);
}
#[test]
fn preferred_ipv6_index_preserves_zone_binding_when_indices_differ() {
    assert_eq!(normalize(vec![adapter()]).interfaces[0].index, 9);
    let mut input = adapter();
    input.index_v6 = 0;
    assert_eq!(normalize(vec![input]).interfaces[0].index, 7);
    let mut input = adapter();
    input.index_v4 = 0;
    assert_eq!(normalize(vec![input]).interfaces[0].index, 9);
}
#[test]
fn address_bytes_and_link_locals_survive_with_scope_mismatch_counts_only() {
    let v4 = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
    let local = IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1));
    let mut input = adapter();
    input.addrs = vec![(local, 9), (v4, 0), (local, 9)];
    let output = normalize(vec![input]);
    assert_eq!(output.scope_mismatches, 0);
    assert_eq!(output.interfaces[0].addrs, vec![v4, local]);
    let mut input = adapter();
    input.addrs = vec![(local, 77)];
    let output = normalize(vec![input]);
    assert_eq!(output.scope_mismatches, 1);
    assert_eq!(output.interfaces[0].addrs, vec![local]);
    let debug = format!("{output:?}");
    assert!(!debug.contains("fe80"));
}
#[test]
fn normalization_is_order_independent_and_sorted_by_os_index() {
    let a = adapter();
    let mut b = adapter();
    b.name = "second".into();
    b.index_v6 = 3;
    b.index_v4 = 3;
    let first = normalize(vec![a, b]).interfaces;
    let a = adapter();
    let mut b = adapter();
    b.name = "second".into();
    b.index_v6 = 3;
    b.index_v4 = 3;
    assert_eq!(first, normalize(vec![b, a]).interfaces);
    assert_eq!(
        first.iter().map(|i| i.index).collect::<Vec<_>>(),
        vec![3, 9]
    );
}
#[test]
fn mtu_speed_and_oper_status_keep_native_unknown_sentinels() {
    assert_eq!(
        normalize(vec![adapter()]).interfaces[0].speed_mbps,
        Some(100)
    );
    let mut input = adapter();
    input.mtu = u32::MAX;
    input.receive_bps = 0;
    input.transmit_bps = u64::MAX;
    input.oper_status = 5;
    let output = normalize(vec![input]);
    assert_eq!(output.interfaces[0].mtu, None);
    assert_eq!(output.interfaces[0].speed_mbps, None);
    assert!(!output.interfaces[0].up);
    let mut input = adapter();
    input.mtu = 0;
    input.transmit_bps = u64::MAX;
    assert_eq!(normalize(vec![input]).interfaces[0].speed_mbps, Some(1000));
}
#[test]
fn initial_and_replacement_always_deliver_current_before_changes() {
    let mut c = Coalescer::default();
    assert_eq!(c.initial(snapshot(true)), snapshot(true));
    assert_eq!(c.initial(snapshot(true)), snapshot(true));
    assert_eq!(c.observed(snapshot(true)), None);
    assert_eq!(c.observed(snapshot(false)), Some(snapshot(false)));
}
#[test]
fn notification_burst_waits_250ms_after_last_signal_and_suppresses_duplicates() {
    let mut c = Coalescer::default();
    c.initial(snapshot(true));
    c.signal(100);
    c.signal(200);
    assert!(!c.due(449));
    assert!(c.due(450));
    assert_eq!(c.observed(snapshot(false)), Some(snapshot(false)));
    assert!(!c.due(1000));
    c.signal(1000);
    assert!(!c.due(1249));
    assert!(c.due(1250));
    assert_eq!(c.observed(snapshot(false)), None);
}
#[test]
fn empty_observation_then_recovery_is_a_full_changed_snapshot() {
    let mut c = Coalescer::default();
    c.initial(snapshot(true));
    assert_eq!(c.observed(Vec::new()), Some(Vec::new()));
    assert_eq!(c.observed(Vec::new()), None);
    assert_eq!(c.observed(snapshot(true)), Some(snapshot(true)));
}
