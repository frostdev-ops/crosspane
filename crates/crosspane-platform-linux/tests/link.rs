//! Read-only host checks and isolated sysfs fixtures for the Linux link classifier.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crosspane_platform::{Interface, LinkClass, LinkInfo, PlatformError};
use crosspane_platform_linux::link::SysfsLinkInfo;

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct SysfsFixture {
    root: PathBuf,
}

impl SysfsFixture {
    fn new() -> Self {
        loop {
            let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("crosspane-link-test-{}-{id}", std::process::id()));
            match fs::create_dir(&root) {
                Ok(()) => {
                    fs::create_dir(root.join("net")).unwrap();
                    fs::create_dir(root.join("devices")).unwrap();
                    return Self { root };
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("create sysfs fixture: {error}"),
            }
        }
    }

    fn net(&self) -> PathBuf {
        self.root.join("net")
    }

    fn interface(&self, name: &str, index: u32, kind: u32, state: &str) -> PathBuf {
        let path = self.net().join(name);
        fs::create_dir(&path).unwrap();
        fs::write(path.join("ifindex"), format!("{index}\n")).unwrap();
        fs::write(path.join("type"), format!("{kind}\n")).unwrap();
        fs::write(path.join("operstate"), format!("{state}\n")).unwrap();
        fs::write(path.join("mtu"), "1500\n").unwrap();
        path
    }

    fn device(&self, interface: &Path, driver: &str) {
        let device = self
            .root
            .join("devices")
            .join(interface.file_name().unwrap());
        fs::create_dir(&device).unwrap();
        symlink(&device, interface.join("device")).unwrap();
        // read_link identifies the driver without needing its target to be mounted.
        symlink(
            Path::new("/sys/bus/test/drivers").join(driver),
            device.join("driver"),
        )
        .unwrap();
    }
}

impl Drop for SysfsFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn named<'a>(interfaces: &'a [Interface], name: &str) -> &'a Interface {
    interfaces
        .iter()
        .find(|interface| interface.name == name)
        .unwrap()
}

#[test]
fn fake_sysfs_classifies_interfaces_and_metadata() {
    let fixture = SysfsFixture::new();
    let tb = fixture.interface("wp-tb", 50, 32, "up");
    fixture.device(&tb, "thunderbolt-net");
    fs::create_dir(tb.join("wireless")).unwrap();
    fs::write(tb.join("mtu"), "9000\n").unwrap();
    fs::write(tb.join("speed"), "20000\n").unwrap();
    let tb_alt = fixture.interface("wp-tb-alt", 51, 1, "down");
    fixture.device(&tb_alt, "thunderbolt_net");

    let wifi = fixture.interface("wp-wifi", 60, 1, "up");
    fs::create_dir(wifi.join("wireless")).unwrap();
    fs::write(wifi.join("speed"), "-1\n").unwrap();
    let phy = fixture.interface("wp-phy", 61, 1, "down");
    symlink(fixture.root.join("devices"), phy.join("phy80211")).unwrap();

    let ethernet = fixture.interface("wp-eth", 70, 1, "up");
    fixture.device(&ethernet, "r8169");
    fs::write(ethernet.join("speed"), "1000\n").unwrap();
    fixture.interface("wp-br", 80, 1, "down");
    let other_type = fixture.interface("wp-type", 81, 65534, "up");
    fixture.device(&other_type, "test-driver");

    let unknown = fixture.interface("wp-unknown", 82, 1, "unknown");
    fs::write(unknown.join("speed"), "0\n").unwrap();
    fs::remove_file(unknown.join("ifindex")).unwrap();
    fs::remove_file(unknown.join("mtu")).unwrap();
    let malformed = fixture.interface("wp-malformed", 83, 1, "down");
    fixture.device(&malformed, "test-driver");
    for attribute in ["type", "ifindex", "mtu", "speed"] {
        fs::write(malformed.join(attribute), "invalid\n").unwrap();
    }
    fs::remove_file(malformed.join("operstate")).unwrap();

    fixture.interface("lo", 1, 772, "unknown");
    fixture.interface("wp-loop", 90, 772, "up");
    fs::write(fixture.net().join("not-an-interface"), "ignored").unwrap();

    let backend = SysfsLinkInfo::with_root(fixture.net());
    let interfaces = backend.interfaces().unwrap();
    for (name, index, class, up, mtu, speed) in [
        (
            "wp-tb",
            50,
            LinkClass::DirectUsb4Tb,
            true,
            Some(9000),
            Some(20000),
        ),
        (
            "wp-tb-alt",
            51,
            LinkClass::DirectUsb4Tb,
            false,
            Some(1500),
            None,
        ),
        ("wp-wifi", 60, LinkClass::Wifi, true, Some(1500), None),
        ("wp-phy", 61, LinkClass::Wifi, false, Some(1500), None),
        ("wp-eth", 70, LinkClass::Lan, true, Some(1500), Some(1000)),
        ("wp-br", 80, LinkClass::Unknown, false, Some(1500), None),
        ("wp-type", 81, LinkClass::Unknown, true, Some(1500), None),
        ("wp-unknown", 0, LinkClass::Unknown, false, None, None),
        ("wp-malformed", 0, LinkClass::Unknown, false, None, None),
    ] {
        assert_eq!(
            named(&interfaces, name),
            &Interface {
                name: name.into(),
                index,
                class,
                up,
                mtu,
                speed_mbps: speed,
                addrs: Vec::new(),
            }
        );
    }
    assert!(
        interfaces
            .windows(2)
            .all(|pair| pair[0].index <= pair[1].index)
    );
    assert!(interfaces.iter().all(|interface| {
        interface.name != "lo"
            && interface.name != "wp-loop"
            && interface.name != "not-an-interface"
            && interface.class != LinkClass::DirectEthernet
    }));

    // with_root retains host addresses, including names absent from the fixture tree.
    let mut host_addresses = BTreeMap::new();
    for address in if_addrs::get_if_addrs().unwrap() {
        if address.name != "lo" {
            host_addresses
                .entry(address.name.clone())
                .or_insert_with(Vec::new)
                .push(address.ip());
        }
    }
    for (name, mut expected) in host_addresses {
        expected.sort_unstable();
        expected.dedup();
        assert_eq!(named(&interfaces, &name).addrs, expected);
    }
    if let Some(address) = if_addrs::get_if_addrs()
        .unwrap()
        .into_iter()
        .find(|address| address.name != "lo" && address.index.is_some())
    {
        let path = fixture.interface(&address.name, 4000, 1, "unknown");
        let interfaces = backend.interfaces().unwrap();
        assert_eq!(named(&interfaces, &address.name).index, 4000);
        assert!(named(&interfaces, &address.name).up);
        fs::remove_file(path.join("ifindex")).unwrap();
        assert_eq!(
            named(&backend.interfaces().unwrap(), &address.name).index,
            address.index.unwrap()
        );
    }
}

#[test]
fn real_machine_interfaces_with_addresses() {
    let interfaces = match SysfsLinkInfo::new().interfaces() {
        Ok(interfaces) => interfaces,
        Err(error) => {
            if fs::read_dir("/sys/class/net").is_err() || if_addrs::get_if_addrs().is_err() {
                eprintln!("skipped: sandbox hides network interfaces: {error}");
                return;
            }
            panic!("enumerate host interfaces: {error}");
        }
    };
    println!("NAME           INDEX CLASS          UP    MTU   SPEED_MBPS ADDRESSES");
    for interface in &interfaces {
        println!(
            "{:<14} {:<5} {:<14?} {:<5} {:<5} {:<10} {}",
            interface.name,
            interface.index,
            interface.class,
            interface.up,
            interface
                .mtu
                .map_or_else(|| "-".into(), |mtu| mtu.to_string()),
            interface
                .speed_mbps
                .map_or_else(|| "-".into(), |speed| speed.to_string()),
            interface
                .addrs
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !interfaces
        .iter()
        .any(|interface| !interface.addrs.is_empty())
    {
        eprintln!("skipped: sandbox exposes no non-loopback interface with an address");
        return;
    }
    assert!(interfaces.iter().all(|interface| interface.name != "lo"));
    assert!(interfaces.iter().any(|interface| {
        !interface.addrs.is_empty() && interface.addrs.iter().any(|address| !address.is_loopback())
    }));
}

#[test]
fn subscribe_delivers_snapshots_and_drop_stops_worker() {
    let fixture = SysfsFixture::new();
    let path = fixture.interface("wp-sub", 100, 1, "up");
    let mut backend = SysfsLinkInfo::with_root(fixture.net());
    let expected = backend.interfaces().unwrap();
    let (send, events) = mpsc::channel();
    let caller = thread::current().id();
    let start = Instant::now();
    backend
        .subscribe(Arc::new(move |event| {
            let _ = send.send((thread::current().id(), event));
        }))
        .unwrap();
    let (delivery_thread, first) = events.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_ne!(delivery_thread, caller);
    assert_eq!(first, expected);
    assert!(matches!(
        backend.subscribe(Arc::new(|_| {})),
        Err(PlatformError::Unsupported(_))
    ));
    assert!(matches!(
        events.recv_timeout(Duration::from_millis(2200)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));

    fs::write(path.join("mtu"), "9000\n").unwrap();
    let (_, changed) = events.recv_timeout(Duration::from_secs(3)).unwrap();
    let mut expected = first;
    expected
        .iter_mut()
        .find(|interface| interface.name == "wp-sub")
        .unwrap()
        .mtu = Some(9000);
    assert_eq!(changed, expected);

    // A failed tick sends no incomplete snapshot, and observation resumes after recovery.
    let offline = fixture.root.join("offline");
    fs::rename(fixture.net(), &offline).unwrap();
    assert!(matches!(
        events.recv_timeout(Duration::from_millis(2200)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    fs::write(offline.join("wp-sub/speed"), "1000\n").unwrap();
    fs::rename(&offline, fixture.net()).unwrap();
    let (_, recovered) = events.recv_timeout(Duration::from_secs(3)).unwrap();
    expected
        .iter_mut()
        .find(|interface| interface.name == "wp-sub")
        .unwrap()
        .speed_mbps = Some(1000);
    assert_eq!(recovered, expected);

    // A timeout detects a hung Drop without hanging the test itself.
    let (dropped, finished) = mpsc::channel();
    thread::spawn(move || {
        drop(backend);
        dropped.send(()).unwrap();
    });
    finished.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(
        events.try_recv(),
        Err(mpsc::TryRecvError::Disconnected)
    ));
}
