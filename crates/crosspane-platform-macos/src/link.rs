//! Read-only interface snapshots classified by macOS hardware port names.

use std::collections::{BTreeMap, BTreeSet};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crosspane_platform::{EventSink, Interface, LinkClass, LinkInfo, PlatformError};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const PORT_CACHE_INTERVAL: Duration = Duration::from_secs(30);
const TOOL_TIMEOUT: Duration = Duration::from_secs(2);
const TOOL_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, Default)]
struct PortCache {
    ports: BTreeMap<String, String>,
    names: BTreeSet<String>,
    refreshed: Option<Instant>,
}

/// Enumerates addressed interfaces without requesting privileges or TCC permissions.
#[derive(Debug, Default)]
pub struct MacLinkInfo {
    ports: Arc<Mutex<PortCache>>,
    stopped: Arc<AtomicBool>,
    subscription: Option<(mpsc::Sender<()>, JoinHandle<()>)>,
}

impl MacLinkInfo {
    pub fn new() -> MacLinkInfo {
        Self::default()
    }
}

impl LinkInfo for MacLinkInfo {
    fn interfaces(&self) -> Result<Vec<Interface>, PlatformError> {
        snapshot(&self.ports, &self.stopped)
    }

    /// A second call returns `PlatformError::Backend` and keeps the original subscription.
    fn subscribe(&mut self, sink: Arc<dyn EventSink<Vec<Interface>>>) -> Result<(), PlatformError> {
        if self.subscription.is_some() {
            return Err(PlatformError::Backend(
                "LinkInfo::subscribe called twice".into(),
            ));
        }
        let (tx, rx) = mpsc::channel();
        let ports = self.ports.clone();
        let stopped = self.stopped.clone();
        let worker = thread::Builder::new()
            .name("mac-links".into())
            .spawn(move || {
                let mut last = None;
                while !stopped.load(Ordering::Acquire) {
                    let tick = Instant::now();
                    match snapshot(&ports, &stopped) {
                        Ok(interfaces) => {
                            if stopped.load(Ordering::Acquire) {
                                break;
                            }
                            if last.as_ref() != Some(&interfaces) {
                                last = Some(interfaces.clone());
                                sink.send(interfaces);
                            }
                        }
                        Err(error) => tracing::debug!(%error, "link snapshot failed"),
                    }
                    match rx.recv_timeout(POLL_INTERVAL.saturating_sub(tick.elapsed())) {
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        _ => break,
                    }
                }
            })
            .map_err(|error| PlatformError::Backend(format!("spawn link thread: {error}")))?;
        self.subscription = Some((tx, worker));
        Ok(())
    }
}

impl Drop for MacLinkInfo {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some((tx, worker)) = self.subscription.take() {
            let _ = tx.send(());
            // A sink may drop its own backend; joining the current thread would panic.
            if worker.thread().id() != thread::current().id() && worker.join().is_err() {
                tracing::debug!("link thread panicked");
            }
        }
    }
}

fn snapshot(
    ports: &Mutex<PortCache>,
    stopped: &AtomicBool,
) -> Result<Vec<Interface>, PlatformError> {
    let addresses = if_addrs::get_if_addrs()
        .map_err(|error| PlatformError::Backend(format!("enumerate interfaces: {error}")))?;
    let mut grouped = BTreeMap::<String, Interface>::new();
    for address in addresses {
        if address.name == "lo0" {
            continue;
        }
        let ip = address.ip();
        let up = address.is_oper_up();
        let index = address.index;
        let interface = grouped
            .entry(address.name.clone())
            .or_insert_with(|| Interface {
                name: address.name,
                index: index.unwrap_or(0),
                class: LinkClass::Unknown,
                up: false,
                mtu: None,
                speed_mbps: None,
                addrs: Vec::new(),
            });
        if let Some(index) = index {
            interface.index = index;
        }
        interface.up |= up;
        interface.addrs.push(ip);
    }

    // Never wait behind another caller's tool lookup: the watcher retries on its next tick.
    let mut cache = ports.try_lock().map_err(|error| match error {
        TryLockError::WouldBlock => PlatformError::Timeout,
        TryLockError::Poisoned(_) => PlatformError::Backend("link port cache poisoned".into()),
    })?;
    let names: BTreeSet<_> = grouped.keys().cloned().collect();
    if cache
        .refreshed
        .is_none_or(|at| at.elapsed() >= PORT_CACHE_INTERVAL)
        || !names.is_subset(&cache.names)
    {
        cache.ports = match read_hardware_ports(stopped) {
            Ok(ports) => ports,
            Err(error) => {
                tracing::debug!(%error, "hardware port lookup failed; using unknown link classes");
                BTreeMap::new()
            }
        };
        // Failed lookups also respect the cache interval; a new name always forces a retry.
        cache.refreshed = Some(Instant::now());
    }
    cache.names = names;

    let mut interfaces: Vec<_> = grouped.into_values().collect();
    for interface in &mut interfaces {
        interface.class = classify(
            &interface.name,
            cache.ports.get(&interface.name).map(String::as_str),
        );
        // Address enumeration order is not a link change.
        interface.addrs.sort_unstable();
        interface.addrs.dedup();
    }
    interfaces.sort_by_key(|interface| interface.index);
    Ok(interfaces)
}

fn read_hardware_ports(stopped: &AtomicBool) -> Result<BTreeMap<String, String>, PlatformError> {
    let started = Instant::now();
    let mut child = Command::new("/usr/sbin/networksetup")
        .arg("-listallhardwareports")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| PlatformError::Backend(format!("start networksetup: {error}")))?;
    loop {
        if stopped.load(Ordering::Acquire) || started.elapsed() >= TOOL_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(PlatformError::Timeout);
        }
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => thread::sleep(
                TOOL_POLL_INTERVAL.min(TOOL_TIMEOUT.saturating_sub(started.elapsed())),
            ),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(PlatformError::Backend(format!(
                    "wait for networksetup: {error}"
                )));
            }
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| PlatformError::Backend(format!("read networksetup output: {error}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let ports = parse_hardware_ports(&stdout);
    // networksetup can report AuthorizationCreate failure on stdout with a zero exit status.
    if !output.status.success() || ports.is_empty() {
        return Err(PlatformError::Backend(format!(
            "networksetup returned {} without usable hardware ports: {} {}",
            output.status,
            stdout.trim(),
            stderr.trim()
        )));
    }
    Ok(ports)
}

fn parse_hardware_ports(output: &str) -> BTreeMap<String, String> {
    let mut ports = BTreeMap::new();
    let mut port = None;
    for line in output.lines().map(str::trim) {
        if line.is_empty() {
            port = None;
        } else if let Some(name) = line.strip_prefix("Hardware Port:") {
            port = Some(name.trim()).filter(|name| !name.is_empty());
        } else if let Some(device) = line.strip_prefix("Device:")
            && let Some(name) = port.take()
            && !device.trim().is_empty()
        {
            ports.insert(device.trim().into(), name.into());
        }
    }
    ports
}

fn classify(_name: &str, port: Option<&str>) -> LinkClass {
    match port {
        Some(port) if port.starts_with("Thunderbolt Ethernet") => LinkClass::Lan,
        Some(port) if port.starts_with("Thunderbolt") => LinkClass::DirectUsb4Tb,
        Some("Wi-Fi" | "AirPort") => LinkClass::Wifi,
        Some(port)
            if ["Ethernet", "LAN", "USB"]
                .iter()
                .any(|word| port.contains(word)) =>
        {
            LinkClass::Lan
        }
        _ => LinkClass::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sample_and_ethernet_adapters() {
        let output = "Hardware Port: Wi-Fi\nDevice: en0\nEthernet Address: …\n\n\
                      Hardware Port: Thunderbolt Bridge\nDevice: bridge0\nEthernet Address: …\n\n\
                      Hardware Port: Thunderbolt 1\nDevice: en1\n\n\
                      Hardware Port: USB 10/100/1000 LAN\nDevice: en5\n\n\
                      Hardware Port: Thunderbolt Ethernet Slot 0\nDevice: en6\n\n\
                      Hardware Port: Ethernet\nDevice: en7\n";
        assert_eq!(
            parse_hardware_ports(output),
            BTreeMap::from([
                ("en0".into(), "Wi-Fi".into()),
                ("bridge0".into(), "Thunderbolt Bridge".into()),
                ("en1".into(), "Thunderbolt 1".into()),
                ("en5".into(), "USB 10/100/1000 LAN".into()),
                ("en6".into(), "Thunderbolt Ethernet Slot 0".into()),
                ("en7".into(), "Ethernet".into()),
            ])
        );
    }

    #[test]
    fn ignores_empty_and_garbage_output() {
        for output in [
            "",
            "garbage\nAuthorizationCreate() failed: -60008\nDevice: en0",
            "Hardware Port: \nDevice: en0\nHardware Port: Wi-Fi\nDevice: ",
            "Hardware Port: Wi-Fi\n\nDevice: en0",
        ] {
            assert!(parse_hardware_ports(output).is_empty(), "{output:?}");
        }
    }

    #[test]
    fn classifies_every_rule() {
        for (name, port, expected) in [
            (
                "bridge0",
                Some("Thunderbolt Bridge"),
                LinkClass::DirectUsb4Tb,
            ),
            ("en1", Some("Thunderbolt 1"), LinkClass::DirectUsb4Tb),
            ("en0", Some("Wi-Fi"), LinkClass::Wifi),
            ("en0", Some("AirPort"), LinkClass::Wifi),
            ("en5", Some("Ethernet"), LinkClass::Lan),
            ("en5", Some("USB 10/100/1000 LAN"), LinkClass::Lan),
            ("en5", Some("Thunderbolt Ethernet Slot 0"), LinkClass::Lan),
            ("en5", Some("LAN"), LinkClass::Lan),
            ("en5", Some("USB Adapter"), LinkClass::Lan),
            ("utun0", None, LinkClass::Unknown),
            ("utun0", Some("VPN"), LinkClass::Unknown),
            ("awdl0", None, LinkClass::Unknown),
            ("llw0", None, LinkClass::Unknown),
            ("anpi0", None, LinkClass::Unknown),
            ("ap1", None, LinkClass::Unknown),
            ("bridge1", None, LinkClass::Unknown),
            ("bridge0", None, LinkClass::Unknown),
            ("bridge1", Some("Bridge"), LinkClass::Unknown),
            ("en0", None, LinkClass::Unknown),
            ("en9", Some("Other"), LinkClass::Unknown),
        ] {
            assert_eq!(classify(name, port), expected, "{name}, {port:?}");
        }
    }
}
