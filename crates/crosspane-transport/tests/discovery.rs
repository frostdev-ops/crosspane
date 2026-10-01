//! mDNS discovery on a real network interface (WP-1.6).
//!
//! The tests start several [`Discovery`]s in one process; they find each other through the host's
//! multicast loopback. Other nodes on the LAN (and tests running in parallel) can be seen too, so
//! every node is recognised by its own advertised port, never by counting candidates.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crosspane_transport::discovery::{Candidate, Discovery, DiscoveryEvent, SERVICE_TYPE};
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent};

/// How long a node may take to see another one (probing alone takes about a second).
const FIND: Duration = Duration::from_secs(10);
/// How long a withdrawn node may take to be reported lost.
const LOSE: Duration = Duration::from_secs(15);

/// Why the environment can't do multicast, or `None` if it can.
fn no_multicast_reason() -> Option<String> {
    // Connecting a UDP socket sends nothing; it only makes the kernel pick the outgoing
    // interface. A route to a non-loopback interface is what mDNS needs.
    for target in ["192.0.2.1:9", "224.0.0.251:5353"] {
        let Ok(socket) = UdpSocket::bind("0.0.0.0:0") else {
            continue;
        };
        if socket.connect(target).is_err() {
            continue;
        }
        if let Ok(local) = socket.local_addr()
            && !local.ip().is_loopback()
            && !local.ip().is_unspecified()
        {
            return None;
        }
    }
    Some("no non-loopback network interface with a route".to_string())
}

macro_rules! require_multicast {
    () => {
        if let Some(reason) = no_multicast_reason() {
            eprintln!("SKIPPED: multicast unavailable here: {reason}");
            return;
        }
    };
}

/// A port nobody listens on, only advertised. Distinct per test process and per `salt`.
fn port(salt: u16) -> u16 {
    assert!(salt < 16);
    20_000 + (std::process::id() % 2_000) as u16 * 16 + salt
}

/// The events of one `Discovery`, with everything seen so far kept for the failure message.
struct Events {
    rx: mpsc::Receiver<DiscoveryEvent>,
    seen: Vec<DiscoveryEvent>,
}

impl Events {
    /// Waits for an event that satisfies `matches`; earlier events are only logged.
    fn wait_for(
        &mut self,
        timeout: Duration,
        what: &str,
        mut matches: impl FnMut(&DiscoveryEvent) -> bool,
    ) -> DiscoveryEvent {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(event) => {
                    self.seen.push(event.clone());
                    if matches(&event) {
                        return event;
                    }
                }
                Err(_) => panic!(
                    "timed out after {timeout:?} waiting for {what}; saw {:#?}",
                    self.seen
                ),
            }
        }
    }

    fn found(&mut self, what: &str, port: u16) -> Candidate {
        match self.wait_for(FIND, what, |event| is_found(event, port)) {
            DiscoveryEvent::Found(candidate) => candidate,
            other => unreachable!("{other:?}"),
        }
    }

    fn drain(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            self.seen.push(event);
        }
    }
}

fn node(port: u16) -> (Discovery, Events) {
    let (tx, rx) = mpsc::channel();
    let discovery = Discovery::start(
        port,
        Box::new(move |event| {
            let _ = tx.send(event);
        }),
    )
    .expect("discovery starts");
    (
        discovery,
        Events {
            rx,
            seen: Vec::new(),
        },
    )
}

fn advertised_port(candidate: &Candidate) -> bool {
    candidate
        .addrs
        .iter()
        .all(|addr| addr.port() == candidate.addrs[0].port())
}

fn on_port(candidate: &Candidate, port: u16) -> bool {
    !candidate.addrs.is_empty() && advertised_port(candidate) && candidate.addrs[0].port() == port
}

fn is_found(event: &DiscoveryEvent, port: u16) -> bool {
    matches!(event, DiscoveryEvent::Found(candidate) if on_port(candidate, port))
}

fn is_hex_id(id: &str) -> bool {
    id.len() == 16
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Everything reported about the test's own nodes (`ports`) names the stable random id, never a
/// renamed `<id> (N)` form, and each of them is listed exactly once.
fn assert_stable_ids(events: &Events, discovery: &Discovery, ports: &[u16]) {
    for event in &events.seen {
        if let DiscoveryEvent::Found(candidate) = event
            && ports.iter().any(|port| on_port(candidate, *port))
        {
            assert!(
                is_hex_id(&candidate.instance),
                "not the stable id: {candidate:?}"
            );
        }
    }
    let listed = discovery.candidates();
    for port in ports {
        let nodes: Vec<&Candidate> = listed.iter().filter(|c| on_port(c, *port)).collect();
        assert_eq!(
            nodes.len(),
            1,
            "port {port} should be listed once: {listed:?}"
        );
        assert!(
            is_hex_id(&nodes[0].instance),
            "not the stable id: {:?}",
            nodes[0]
        );
    }
}

fn is_link_local_v6(addr: &SocketAddr) -> bool {
    matches!(addr.ip(), IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80)
}

/// What a plain mDNS browser sees of our service, to check the raw advertisement.
struct RawBrowser {
    daemon: ServiceDaemon,
    rx: mdns_sd::Receiver<ServiceEvent>,
}

impl RawBrowser {
    fn new() -> Self {
        let daemon = ServiceDaemon::new().expect("raw daemon");
        let rx = daemon.browse(SERVICE_TYPE).expect("raw browse");
        RawBrowser { daemon, rx }
    }

    fn resolved(
        &self,
        port: u16,
        what: &str,
        mut matches: impl FnMut(&ResolvedService) -> bool,
    ) -> ResolvedService {
        let deadline = Instant::now() + FIND;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(ServiceEvent::ServiceResolved(resolved))
                    if resolved.port == port && matches(&resolved) =>
                {
                    return *resolved;
                }
                Ok(_) => {}
                Err(_) => panic!("timed out waiting for {what}"),
            }
        }
    }
}

impl Drop for RawBrowser {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

fn txt_keys(resolved: &ResolvedService) -> Vec<String> {
    let mut keys: Vec<String> = resolved
        .txt_properties
        .iter()
        .map(|property| property.key().to_string())
        .collect();
    keys.sort();
    keys
}

#[test]
fn two_nodes_see_each_other_and_not_themselves() {
    require_multicast!();
    let (pa, pb) = (port(0), port(1));
    let started = Instant::now();
    let (a, mut a_events) = node(pa);
    let (b, mut b_events) = node(pb);

    let b_seen = a_events.found("A to see B", pb);
    let a_seen = b_events.found("B to see A", pa);
    eprintln!("each saw the other after {:?}", started.elapsed());
    eprintln!("A advertised: {a_seen:?}");
    eprintln!("B advertised: {b_seen:?}");

    for (candidate, expected_port) in [(&b_seen, pb), (&a_seen, pa)] {
        assert_eq!(candidate.version, 1);
        assert_eq!(candidate.pairing_name, None);
        assert!(is_hex_id(&candidate.instance), "{candidate:?}");
        assert!(!candidate.addrs.is_empty());
        for addr in &candidate.addrs {
            assert_eq!(addr.port(), expected_port, "{candidate:?}");
            assert!(!addr.ip().is_loopback(), "{candidate:?}");
            assert!(!addr.ip().is_unspecified(), "{candidate:?}");
            if let SocketAddr::V6(v6) = addr
                && is_link_local_v6(addr)
            {
                assert_ne!(
                    v6.scope_id(),
                    0,
                    "link-local needs its scope: {candidate:?}"
                );
            }
        }
    }
    assert_ne!(a_seen.instance, b_seen.instance);

    // `candidates()` agrees with the events.
    assert!(a.candidates().iter().any(|c| c.instance == b_seen.instance));
    assert!(b.candidates().iter().any(|c| c.instance == a_seen.instance));

    // Neither ever reports itself. Our own advertisement is looped back to us by now (it is
    // announced at about the same time the other node's is), but give it time to be sure.
    std::thread::sleep(Duration::from_secs(2));
    a_events.drain();
    b_events.drain();
    for (name, own_port, own_instance, events, discovery) in [
        ("A", pa, &a_seen.instance, &a_events, &a),
        ("B", pb, &b_seen.instance, &b_events, &b),
    ] {
        for event in &events.seen {
            match event {
                DiscoveryEvent::Found(candidate) => assert!(
                    !on_port(candidate, own_port) && candidate.instance != *own_instance,
                    "{name} saw itself: {candidate:?}"
                ),
                DiscoveryEvent::Lost { instance } => {
                    assert_ne!(instance, own_instance, "{name} lost itself")
                }
            }
        }
        assert!(
            discovery
                .candidates()
                .iter()
                .all(|c| c.instance != *own_instance && !on_port(c, own_port)),
            "{name} lists itself: {:?}",
            discovery.candidates()
        );
    }
    assert_stable_ids(&a_events, &a, &[pb]);
    assert_stable_ids(&b_events, &b, &[pa]);
}

#[test]
fn pairing_name_is_advertised_only_while_set() {
    require_multicast!();
    let (pa, pb) = (port(2), port(3));
    let (a, _a_events) = node(pa);
    let (b, mut b_events) = node(pb);

    let first = b_events.found("B to see A", pa);
    assert_eq!(first.pairing_name, None);

    let started = Instant::now();
    a.set_pairing_name(Some("Desk")).unwrap();
    let named = b_events.wait_for(FIND, "A's pairing name", |event| {
        matches!(event, DiscoveryEvent::Found(c) if on_port(c, pa) && c.pairing_name.is_some())
    });
    eprintln!("name appeared after {:?}", started.elapsed());
    let DiscoveryEvent::Found(named) = named else {
        unreachable!()
    };
    assert_eq!(named.pairing_name.as_deref(), Some("Desk"));
    assert!(is_hex_id(&named.instance), "{named:?}");
    assert_eq!(named.instance, first.instance, "same node, same instance");
    assert_eq!(named.version, 1);

    let started = Instant::now();
    a.set_pairing_name(None).unwrap();
    let unnamed = b_events.wait_for(FIND, "A's pairing name to go", |event| {
        matches!(event, DiscoveryEvent::Found(c) if on_port(c, pa) && c.pairing_name.is_none())
    });
    eprintln!("name went after {:?}", started.elapsed());
    let DiscoveryEvent::Found(unnamed) = unnamed else {
        unreachable!()
    };
    assert!(is_hex_id(&unnamed.instance), "{unnamed:?}");
    assert_eq!(unnamed.instance, first.instance);
    b_events.drain();
    assert_stable_ids(&b_events, &b, &[pa]);
    assert!(
        b_events
            .seen
            .iter()
            .all(|event| !matches!(event, DiscoveryEvent::Lost { instance } if *instance == first.instance)),
        "a name change is not a loss: {:#?}",
        b_events.seen
    );
    let listed = b.candidates();
    assert!(
        listed
            .iter()
            .any(|c| c.instance == first.instance && c.pairing_name.is_none()),
        "{listed:?}"
    );
}

#[test]
fn rapid_pairing_name_changes_end_in_the_last_one() {
    require_multicast!();
    let (pa, pb) = (port(12), port(13));
    let (a, _a_events) = node(pa);
    let (b, mut b_events) = node(pb);
    let first = b_events.found("B to see A", pa);

    // Changes closer together than the spacing mdns-sd's browsers can digest are held back and
    // merged; the network ends up with the last one.
    a.set_pairing_name(Some("One")).unwrap();
    a.set_pairing_name(None).unwrap();
    a.set_pairing_name(Some("Two")).unwrap();
    a.set_pairing_name(Some("Three")).unwrap();
    let last = b_events.wait_for(FIND, "A's last pairing name", |event| {
        matches!(event, DiscoveryEvent::Found(c)
            if on_port(c, pa) && c.pairing_name.as_deref() == Some("Three"))
    });
    eprintln!("last name seen after the events {:#?}", b_events.seen);
    let DiscoveryEvent::Found(last) = last else {
        unreachable!()
    };
    assert!(is_hex_id(&last.instance), "{last:?}");
    assert_eq!(last.instance, first.instance);
    b_events.drain();
    assert_stable_ids(&b_events, &b, &[pa]);
    assert!(
        b_events
            .seen
            .iter()
            .all(|event| !matches!(event, DiscoveryEvent::Lost { instance } if *instance == first.instance)),
        "{:#?}",
        b_events.seen
    );
    // Nothing the caller never asked for was advertised.
    for event in &b_events.seen {
        if let DiscoveryEvent::Found(c) = event {
            if c.instance != first.instance {
                continue;
            }
            assert!(
                matches!(
                    c.pairing_name.as_deref(),
                    None | Some("One") | Some("Three")
                ),
                "{c:?}"
            );
        }
    }
    let listed = b.candidates();
    assert!(
        listed
            .iter()
            .any(|c| c.instance == first.instance && c.pairing_name.as_deref() == Some("Three")),
        "{listed:?}"
    );
}

#[test]
fn long_pairing_names_are_cut_on_a_character_boundary() {
    require_multicast!();
    let (pa, pb) = (port(4), port(5));
    let (a, _a_events) = node(pa);
    let (b, mut b_events) = node(pb);
    b_events.found("B to see A", pa);

    // 200 bytes of two-byte characters: 31 of them fit in 63 bytes.
    a.set_pairing_name(Some(&"é".repeat(100))).unwrap();
    let named = b_events.wait_for(FIND, "A's cut name", |event| {
        matches!(event, DiscoveryEvent::Found(c) if on_port(c, pa) && c.pairing_name.is_some())
    });
    let DiscoveryEvent::Found(named) = named else {
        unreachable!()
    };
    assert_eq!(named.pairing_name, Some("é".repeat(31)));
    assert!(is_hex_id(&named.instance), "{named:?}");
    b_events.drain();
    assert_stable_ids(&b_events, &b, &[pa]);
}

#[test]
fn dropping_a_node_withdraws_it() {
    require_multicast!();
    let (pa, pb) = (port(6), port(7));
    let (a, _a_events) = node(pa);
    let (b, mut b_events) = node(pb);
    let seen = b_events.found("B to see A", pa);

    let started = Instant::now();
    drop(a);
    let dropped_in = started.elapsed();
    eprintln!("drop took {dropped_in:?}");
    assert!(
        dropped_in < Duration::from_secs(2),
        "drop took {dropped_in:?}"
    );

    let lost = b_events.wait_for(
        LOSE,
        "A to be lost",
        |event| matches!(event, DiscoveryEvent::Lost { instance } if *instance == seen.instance),
    );
    eprintln!("lost {:?} after {:?}", lost, started.elapsed());
    assert_eq!(
        lost,
        DiscoveryEvent::Lost {
            instance: seen.instance.clone()
        }
    );
    assert!(
        b.candidates().iter().all(|c| c.instance != seen.instance),
        "{:?}",
        b.candidates()
    );
}

#[test]
fn dropping_does_not_hang_on_a_callback_that_never_returns() {
    require_multicast!();
    let (pa, pb) = (port(14), port(15));
    // Misuse: a callback that blocks, which the API forbids. Dropping must still return.
    let (entered_tx, entered_rx) = mpsc::channel();
    let stuck = Discovery::start(
        pb,
        Box::new(move |_| {
            let _ = entered_tx.send(());
            std::thread::sleep(Duration::from_secs(4));
        }),
    )
    .expect("discovery starts");
    let (_a, _a_events) = node(pa);
    entered_rx
        .recv_timeout(FIND)
        .expect("the stuck node is called for the other one");

    let started = Instant::now();
    drop(stuck);
    let dropped_in = started.elapsed();
    eprintln!("drop with a stuck callback took {dropped_in:?}");
    assert!(
        dropped_in < Duration::from_secs(2),
        "drop took {dropped_in:?}"
    );
}

#[test]
fn instance_ids_are_random_hex_and_change_on_restart() {
    require_multicast!();
    let (pw, pa, pb) = (port(8), port(9), port(10));
    // One watcher sees two nodes, then a restart of the first.
    let (watcher, mut events) = node(pw);
    let (a, _a_events) = node(pa);
    let first = events.found("the watcher to see A", pa);
    assert!(is_hex_id(&first.instance), "{first:?}");

    let (b, _b_events) = node(pb);
    let second = events.found("the watcher to see B", pb);
    assert!(is_hex_id(&second.instance), "{second:?}");
    assert_ne!(first.instance, second.instance);

    drop(a);
    events.wait_for(
        LOSE,
        "A to be lost",
        |event| matches!(event, DiscoveryEvent::Lost { instance } if *instance == first.instance),
    );
    let (_a2, _a2_events) = node(pa);
    let restarted = events.found("the watcher to see A again", pa);
    assert!(is_hex_id(&restarted.instance), "{restarted:?}");
    assert_ne!(restarted.instance, first.instance);
    assert_ne!(restarted.instance, second.instance);
    events.drain();
    assert_stable_ids(&events, &watcher, &[pa, pb]);
    drop(b);
}

#[test]
fn the_advertisement_has_nothing_human_readable() {
    require_multicast!();
    let pa = port(11);
    let raw = RawBrowser::new();
    let (a, _a_events) = node(pa);

    // Normal mode: a random instance, a host name made from it, and the version, nothing else.
    let plain = raw.resolved(pa, "A's raw advertisement", |_| true);
    let suffix = format!(".{SERVICE_TYPE}");
    let instance = plain
        .fullname
        .strip_suffix(&suffix)
        .unwrap_or_else(|| panic!("{}", plain.fullname));
    assert!(is_hex_id(instance), "{}", plain.fullname);
    assert_eq!(plain.host, format!("{instance}.local."));
    assert_eq!(txt_keys(&plain), ["v"]);
    assert_eq!(plain.txt_properties.get_property_val_str("v"), Some("1"));
    assert_eq!(plain.ty_domain, SERVICE_TYPE);
    eprintln!("raw advertisement: {plain:?}");

    // Pairing mode adds exactly the name.
    a.set_pairing_name(Some("Desk")).unwrap();
    let named = raw.resolved(pa, "A's raw pairing advertisement", |resolved| {
        resolved.txt_properties.get("n").is_some()
    });
    assert_eq!(txt_keys(&named), ["n", "v"]);
    assert_eq!(named.txt_properties.get_property_val_str("n"), Some("Desk"));
    assert_eq!(named.fullname, plain.fullname);
    assert_eq!(named.host, plain.host);
}
