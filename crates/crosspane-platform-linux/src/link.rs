//! Read-only Linux network interface enumeration and sysfs link classification.

use std::collections::BTreeMap;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crosspane_platform::{EventSink, Interface, LinkClass, LinkInfo, PlatformError};

const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Classifies interfaces using `/sys/class/net` and groups their IP addresses by name.
#[derive(Debug)]
pub struct SysfsLinkInfo {
    sysfs: PathBuf,
    subscription: Option<Subscription>,
}

#[derive(Debug)]
struct Subscription {
    stop: mpsc::Sender<()>,
    worker: JoinHandle<()>,
}

impl SysfsLinkInfo {
    /// Use the host's network sysfs tree. No observation thread starts until subscription.
    pub fn new() -> SysfsLinkInfo {
        Self::with_root(PathBuf::from("/sys/class/net"))
    }

    /// Use a different sysfs tree; addresses still come from the host's interfaces.
    pub fn with_root(sysfs: PathBuf) -> SysfsLinkInfo {
        Self {
            sysfs,
            subscription: None,
        }
    }
}

impl Default for SysfsLinkInfo {
    fn default() -> Self {
        Self::new()
    }
}

impl LinkInfo for SysfsLinkInfo {
    fn interfaces(&self) -> Result<Vec<Interface>, PlatformError> {
        snapshot(&self.sysfs)
    }

    /// Deliver the first snapshot from the worker, then poll for changes every two seconds.
    /// A second subscription returns [`PlatformError::Unsupported`].
    fn subscribe(&mut self, sink: Arc<dyn EventSink<Vec<Interface>>>) -> Result<(), PlatformError> {
        if self.subscription.is_some() {
            return Err(PlatformError::Unsupported("link info already subscribed"));
        }

        let sysfs = self.sysfs.clone();
        let (stop, stopped) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("crosspane-link-info".into())
            .spawn(move || poll(&sysfs, sink, stopped))
            .map_err(|error| PlatformError::Backend(format!("spawn link observer: {error}")))?;
        self.subscription = Some(Subscription { stop, worker });
        Ok(())
    }
}

impl Drop for SysfsLinkInfo {
    fn drop(&mut self) {
        if let Some(subscription) = self.subscription.take() {
            let _ = subscription.stop.send(());
            if subscription.worker.join().is_err() {
                tracing::debug!("link observer terminated unexpectedly");
            }
        }
    }
}

#[derive(Default)]
struct Addresses {
    index: Option<u32>,
    addrs: Vec<IpAddr>,
}

fn snapshot(sysfs: &Path) -> Result<Vec<Interface>, PlatformError> {
    let mut names = BTreeMap::<String, Addresses>::new();
    let addresses = if_addrs::get_if_addrs().map_err(|error| {
        PlatformError::Backend(format!("enumerate interface addresses: {error}"))
    })?;
    for address in addresses {
        let ip = address.ip();
        let entry = names.entry(address.name).or_default();
        entry.index = entry.index.or(address.index);
        entry.addrs.push(ip);
    }

    let directories = fs::read_dir(sysfs)
        .map_err(|error| PlatformError::Backend(format!("enumerate network sysfs: {error}")))?;
    for directory in directories {
        let directory = directory.map_err(|error| {
            PlatformError::Backend(format!("read network sysfs entry: {error}"))
        })?;
        // Real sysfs entries are symlinks to directories; fixtures may use plain directories.
        if directory.path().is_dir() {
            let name = directory.file_name().into_string().map_err(|_| {
                PlatformError::Backend("network sysfs interface name is not UTF-8".into())
            })?;
            names.entry(name).or_default();
        }
    }

    let mut interfaces = Vec::new();
    for (name, mut addresses) in names {
        let path = sysfs.join(&name);
        let kind = read_number::<u32>(&path.join("type"));
        if name == "lo" || kind == Some(772) {
            continue;
        }

        // Address order from getifaddrs must not create spurious change notifications.
        addresses.addrs.sort_unstable();
        addresses.addrs.dedup();
        let operstate = fs::read_to_string(path.join("operstate")).ok();
        let up = match operstate.as_deref().map(str::trim) {
            Some("up") => true,
            Some("unknown") => !addresses.addrs.is_empty(),
            _ => false,
        };
        interfaces.push(Interface {
            name,
            index: read_number(&path.join("ifindex"))
                .or(addresses.index)
                .unwrap_or(0),
            class: classify(&path, kind),
            up,
            mtu: read_number(&path.join("mtu")),
            speed_mbps: read_number::<u64>(&path.join("speed")).filter(|speed| *speed > 0),
            addrs: addresses.addrs,
        });
    }
    // BTreeMap already supplies a deterministic name order for equal or unknown indices.
    interfaces.sort_by_key(|interface| interface.index);
    Ok(interfaces)
}

fn read_number<T: FromStr>(path: &Path) -> Option<T> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn classify(path: &Path, kind: Option<u32>) -> LinkClass {
    let driver = fs::read_link(path.join("device/driver")).ok();
    if matches!(
        driver
            .as_deref()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str()),
        Some("thunderbolt-net" | "thunderbolt_net")
    ) {
        return LinkClass::DirectUsb4Tb;
    }
    if path.join("wireless").is_dir() || path.join("phy80211").exists() {
        return LinkClass::Wifi;
    }
    let has_device = fs::symlink_metadata(path.join("device"))
        .is_ok_and(|metadata| metadata.file_type().is_symlink());
    if !has_device || kind != Some(1) {
        return LinkClass::Unknown;
    }
    LinkClass::Lan
}

fn poll(sysfs: &Path, sink: Arc<dyn EventSink<Vec<Interface>>>, stopped: mpsc::Receiver<()>) {
    let mut last = None;
    loop {
        if !matches!(stopped.try_recv(), Err(mpsc::TryRecvError::Empty)) {
            break;
        }
        match snapshot(sysfs) {
            Ok(interfaces) if last.as_ref() != Some(&interfaces) => {
                sink.send(interfaces.clone());
                last = Some(interfaces);
            }
            Ok(_) => {}
            Err(error) => tracing::debug!(%error, "link snapshot failed; retrying next tick"),
        }
        match stopped.recv_timeout(POLL_INTERVAL) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}
