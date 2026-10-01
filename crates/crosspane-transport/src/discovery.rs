//! mDNS/DNS-SD discovery of Crosspane nodes on the local network (03 §2 step 1, 04 §3,
//! WP-1.6).
//!
//! Each node advertises `_crosspane._udp.local.` with a **random instance id** (new at every
//! start) and its protocol version, and **no device name**, so names don't leak on shared Wi-Fi.
//! While a pairing window is open the advertisement also carries the device name, so the other
//! machine can offer it in a list. Paired peers are recognised by their keys in the TLS handshake
//! after dialling a candidate, never by anything in the advertisement.

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash, Hasher};
use std::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mdns_sd::{
    DaemonEvent, IfKind, Receiver, RecvTimeoutError, ResolvedService, ScopedIp, ServiceDaemon,
    ServiceEvent, ServiceInfo,
};

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

/// The protocol major version advertised in the `v` TXT key.
const PROTOCOL_VERSION: u32 = 1;
/// TXT key: protocol major version.
const TXT_VERSION: &str = "v";
/// TXT key: device name, present only while a pairing window is open.
const TXT_NAME: &str = "n";
/// The longest device name carried in the `n` TXT key, in bytes.
const MAX_NAME_BYTES: usize = 63;
/// Nodes tracked at once; the rest are ignored so a hostile network can't grow our memory.
const MAX_CANDIDATES: usize = 256;
/// Addresses kept per node.
const MAX_ADDRS: usize = 32;
/// How often the background thread checks whether it should stop.
const POLL: Duration = Duration::from_millis(100);
/// Dropping a [`Discovery`] takes at most about this long, however the daemon behaves.
const DROP_BUDGET: Duration = Duration::from_millis(1500);
/// The longest single wait on the daemon while dropping.
const DROP_STEP: Duration = Duration::from_millis(500);

/// The least time between two changes of the advertisement.
///
/// mdns-sd's browser keeps a changed TXT record next to the old one, instead of replacing it, when
/// the old one was received less than a second before (RFC 6762 §10.2), and then keeps reporting
/// the old one (it did not recover within the 25 s we waited). A node that has just announced one
/// TXT is heard again about 1.1 s later (the second announcement), so a change needs more than
/// 2.2 s of quiet before it. Measured on the desktop: changes 2.4 s apart always arrived, 2.0 s
/// apart sometimes never did.
const CHANGE_SPACING: Duration = Duration::from_millis(2500);

/// What the background thread and the owner share.
#[derive(Default)]
struct Shared {
    /// The nodes currently seen, by instance id.
    candidates: Mutex<HashMap<String, Candidate>>,
    /// The pairing name this node advertises, and when it last changed.
    advert: Mutex<Advert>,
}

/// The device name in this node's advertisement (pairing mode), wanted and sent.
#[derive(Default)]
struct Advert {
    /// What `set_pairing_name` last asked for (never empty).
    wanted: Option<String>,
    /// What the registered service carries now.
    sent: Option<String>,
    /// When the service was last re-registered with a different name.
    last_change: Option<Instant>,
}

impl Advert {
    /// There is a change to send, and the last one is far enough back.
    fn due(&self) -> bool {
        self.wanted != self.sent
            && self
                .last_change
                .is_none_or(|changed| changed.elapsed() >= CHANGE_SPACING)
    }

    /// Re-registers the service with the wanted name. That announces it again, and mdns-sd does
    /// not probe the already-owned name again.
    fn send(
        &mut self,
        daemon: &ServiceDaemon,
        instance: &str,
        port: u16,
    ) -> Result<(), DiscoveryError> {
        let info = advertisement(instance, port, self.wanted.as_deref(), false)?;
        daemon.register(info).map_err(mdns_error)?;
        self.sent = self.wanted.clone();
        self.last_change = Some(Instant::now());
        Ok(())
    }
}

/// Advertising and browsing, on a background thread. Dropping it withdraws the advertisement.
pub struct Discovery {
    daemon: ServiceDaemon,
    instance: String,
    fullname: String,
    port: u16,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// Disconnects when the background thread has finished, however it finished.
    thread_done: Mutex<mpsc::Receiver<()>>,
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
        let instance = new_instance_id();
        let info = advertisement(&instance, port, None, true)?;
        let fullname = info.get_fullname().to_string();

        let daemon = ServiceDaemon::new().map_err(mdns_error)?;
        match Self::start_on(&daemon, instance, fullname, port, info, events) {
            Ok(discovery) => Ok(discovery),
            Err(error) => {
                // Don't leak the daemon's thread; this also withdraws what was registered.
                let _ = daemon.shutdown();
                Err(error)
            }
        }
    }

    fn start_on(
        daemon: &ServiceDaemon,
        instance: String,
        fullname: String,
        port: u16,
        info: ServiceInfo,
        events: Box<dyn Fn(DiscoveryEvent) + Send + Sync>,
    ) -> Result<Discovery, DiscoveryError> {
        // This node is for other machines: loopback addresses are useless to them. mdns-sd
        // enables loopback interfaces by default.
        daemon
            .disable_interface(IfKind::LoopbackV4)
            .map_err(mdns_error)?;
        daemon
            .disable_interface(IfKind::LoopbackV6)
            .map_err(mdns_error)?;

        let monitor = daemon.monitor().map_err(mdns_error)?;
        let browse = daemon.browse(SERVICE_TYPE).map_err(mdns_error)?;
        daemon.register(info).map_err(mdns_error)?;

        let shared = Arc::new(Shared::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let browser = Browser {
            own: instance.clone(),
            port,
            daemon: daemon.clone(),
            shared: Arc::clone(&shared),
            events,
        };
        let thread = std::thread::Builder::new()
            .name("crosspane-discovery".into())
            .spawn({
                let stop = Arc::clone(&stop);
                move || {
                    // Dropped when the thread ends, which is what `Drop` waits for.
                    let _done = done_tx;
                    browser.run(&browse, &monitor, &stop);
                }
            })
            .map_err(mdns_error)?;

        Ok(Discovery {
            daemon: daemon.clone(),
            instance,
            fullname,
            port,
            shared,
            stop,
            thread: Some(thread),
            thread_done: Mutex::new(done_rx),
        })
    }

    /// While `Some(name)`, the advertisement carries the device name (pairing mode, 04 §3);
    /// `None` withdraws it again.
    ///
    /// The name is cut to 63 bytes at a character boundary. An empty name counts as `None`.
    ///
    /// Changes reach the network at least 2.5 s apart (`CHANGE_SPACING`). A change that
    /// comes sooner than that after the previous one returns `Ok` at once and is sent by the
    /// background thread when its time has come, so only the latest of several rapid changes is
    /// sent at all.
    pub fn set_pairing_name(&self, name: Option<&str>) -> Result<(), DiscoveryError> {
        let wanted = name.map(cut_name).filter(|name| !name.is_empty());
        let mut advert = lock(&self.shared.advert);
        advert.wanted = wanted.map(str::to_string);
        if !advert.due() {
            return Ok(());
        }
        let result = advert.send(&self.daemon, &self.instance, self.port);
        if result.is_err() {
            advert.wanted = advert.sent.clone();
        }
        result
    }

    /// The nodes currently seen, not including this one.
    pub fn candidates(&self) -> Vec<Candidate> {
        let mut all: Vec<Candidate> = lock(&self.shared.candidates).values().cloned().collect();
        all.sort_by(|a, b| a.instance.cmp(&b.instance));
        all
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        let deadline = Instant::now() + DROP_BUDGET;
        let remaining =
            |step: Duration| step.min(deadline.saturating_duration_since(Instant::now()));

        // No more callbacks from here on.
        self.stop.store(true, Ordering::SeqCst);

        // Withdraw the advertisement (goodbye packets), then stop the daemon's thread.
        if let Ok(status) = self.daemon.unregister(&self.fullname) {
            let _ = status.recv_timeout(remaining(DROP_STEP));
        }
        if let Ok(status) = self.daemon.shutdown() {
            let _ = status.recv_timeout(remaining(DROP_STEP));
        }

        // The background thread notices `stop` (or the closed channels) within `POLL`. If it is
        // stuck in the caller's callback, give up on it rather than hang the caller.
        if let Some(handle) = self.thread.take() {
            let finished = !matches!(
                lock(&self.thread_done).recv_timeout(remaining(DROP_BUDGET)),
                Err(mpsc::RecvTimeoutError::Timeout)
            );
            if finished {
                let _ = handle.join();
            }
        }
    }
}

/// The background thread's state: turns mdns-sd events into [`DiscoveryEvent`]s.
struct Browser {
    /// This node's own instance id.
    own: String,
    port: u16,
    daemon: ServiceDaemon,
    shared: Arc<Shared>,
    events: Box<dyn Fn(DiscoveryEvent) + Send + Sync>,
}

impl Browser {
    fn run(
        &self,
        browse: &Receiver<ServiceEvent>,
        monitor: &Receiver<DaemonEvent>,
        stop: &AtomicBool,
    ) {
        while !stop.load(Ordering::SeqCst) {
            match browse.recv_timeout(POLL) {
                Ok(event) => self.on_service_event(event),
                Err(RecvTimeoutError::Timeout) => {}
                // The daemon is gone.
                Err(RecvTimeoutError::Disconnected) => return,
            }
            while let Ok(event) = monitor.try_recv() {
                if let DaemonEvent::Error(error) = event {
                    tracing::warn!(%error, "mDNS daemon error");
                }
            }
            self.send_due_change();
        }
    }

    /// Sends a pairing-name change that `set_pairing_name` had to hold back.
    fn send_due_change(&self) {
        let mut advert = lock(&self.shared.advert);
        if advert.due()
            && let Err(error) = advert.send(&self.daemon, &self.own, self.port)
        {
            tracing::warn!(%error, "could not update the mDNS advertisement");
            // Try again after the spacing, not on every tick.
            advert.last_change = Some(Instant::now());
        }
    }

    fn on_service_event(&self, event: ServiceEvent) {
        match event {
            ServiceEvent::ServiceResolved(resolved) => self.on_resolved(&resolved),
            ServiceEvent::ServiceRemoved(_, fullname) => self.on_removed(&fullname),
            _ => {}
        }
    }

    fn on_resolved(&self, resolved: &ResolvedService) {
        if !resolved.ty_domain.eq_ignore_ascii_case(SERVICE_TYPE) {
            return;
        }
        let Some(instance) = instance_of(&resolved.fullname) else {
            return;
        };
        if is_own(&self.own, instance) {
            return;
        }
        let event = {
            let mut seen = lock(&self.shared.candidates);
            match candidate_from(instance, resolved) {
                Some(candidate) if seen.get(instance) == Some(&candidate) => None,
                Some(_) if !seen.contains_key(instance) && seen.len() >= MAX_CANDIDATES => None,
                Some(candidate) => {
                    seen.insert(instance.to_string(), candidate.clone());
                    Some(DiscoveryEvent::Found(candidate))
                }
                // No longer usable (for example, its `v` disappeared): it is gone for us.
                None => seen.remove(instance).map(|_| DiscoveryEvent::Lost {
                    instance: instance.to_string(),
                }),
            }
        };
        // Not under the lock: the callback may call `candidates()`.
        if let Some(event) = event {
            self.emit(event);
        }
    }

    fn on_removed(&self, fullname: &str) {
        let Some(instance) = instance_of(fullname) else {
            return;
        };
        if is_own(&self.own, instance) {
            return;
        }
        let was_seen = lock(&self.shared.candidates).remove(instance).is_some();
        if was_seen {
            self.emit(DiscoveryEvent::Lost {
                instance: instance.to_string(),
            });
        }
    }

    fn emit(&self, event: DiscoveryEvent) {
        // A panicking callback must not silently end discovery.
        let _ = catch_unwind(AssertUnwindSafe(|| (self.events)(event)));
    }
}

/// `instance` is our own advertisement, or its renamed form after a name conflict (`<id> (2)`).
fn is_own(own: &str, instance: &str) -> bool {
    let Some(prefix) = instance.get(..own.len()) else {
        return false;
    };
    prefix.eq_ignore_ascii_case(own)
        && instance
            .get(own.len()..)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(" ("))
}

/// The instance label of `<instance>.<SERVICE_TYPE>`.
fn instance_of(fullname: &str) -> Option<&str> {
    let split = fullname.len().checked_sub(SERVICE_TYPE.len() + 1)?;
    let instance = fullname.get(..split)?;
    let rest = fullname.get(split..)?;
    let suffix = rest.strip_prefix('.')?;
    (suffix.eq_ignore_ascii_case(SERVICE_TYPE) && !instance.is_empty()).then_some(instance)
}

/// What a resolved service says about its node, or `None` if it is not one of ours (no usable
/// `v`) or has nowhere to be dialled.
fn candidate_from(instance: &str, resolved: &ResolvedService) -> Option<Candidate> {
    let version = resolved
        .txt_properties
        .get(TXT_VERSION)?
        .val()
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse::<u32>().ok())?;
    let pairing_name = resolved
        .txt_properties
        .get(TXT_NAME)
        .and_then(|property| property.val())
        .and_then(|value| std::str::from_utf8(value).ok())
        .map(cut_name)
        .filter(|name| !name.is_empty())
        .map(str::to_string);

    if resolved.port == 0 {
        return None;
    }
    let mut addrs: Vec<SocketAddr> = resolved
        .addresses
        .iter()
        .filter_map(|addr| socket_addr(addr, resolved.port))
        .collect();
    addrs.sort();
    addrs.dedup();
    addrs.truncate(MAX_ADDRS);
    if addrs.is_empty() {
        return None;
    }

    Some(Candidate {
        instance: instance.to_string(),
        addrs,
        version,
        pairing_name,
    })
}

/// A dialable address: not loopback, unspecified or multicast. Link-local IPv6 keeps its scope.
fn socket_addr(addr: &ScopedIp, port: u16) -> Option<SocketAddr> {
    let ip = addr.to_ip_addr();
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return None;
    }
    match (addr, ip) {
        (ScopedIp::V6(scoped), IpAddr::V6(v6)) => {
            let scope = if is_unicast_link_local(&v6) {
                scoped.scope_id().index
            } else {
                0
            };
            Some(SocketAddr::V6(SocketAddrV6::new(v6, port, 0, scope)))
        }
        _ => Some(SocketAddr::new(ip, port)),
    }
}

fn is_unicast_link_local(addr: &Ipv6Addr) -> bool {
    (addr.segments()[0] & 0xffc0) == 0xfe80
}

/// This node's advertisement: no addresses given (mdns-sd tracks the host's), TXT `v` and, in
/// pairing mode, `n`.
///
/// A new name is probed for before it is announced; `probe` is false when re-announcing a name
/// this node already owns.
fn advertisement(
    instance: &str,
    port: u16,
    pairing_name: Option<&str>,
    probe: bool,
) -> Result<ServiceInfo, DiscoveryError> {
    let mut properties = vec![(TXT_VERSION.to_string(), PROTOCOL_VERSION.to_string())];
    if let Some(name) = pairing_name {
        properties.push((TXT_NAME.to_string(), cut_name(name).to_string()));
    }
    let host = format!("{instance}.local.");
    let mut info = ServiceInfo::new(SERVICE_TYPE, instance, &host, "", port, &properties[..])
        .map_err(mdns_error)?;
    info.set_requires_probe(probe);
    Ok(info.enable_addr_auto())
}

/// At most [`MAX_NAME_BYTES`] bytes of `name`, cut at a character boundary.
fn cut_name(name: &str) -> &str {
    let mut end = name.len().min(MAX_NAME_BYTES);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// 16 random lowercase hex characters.
///
/// The randomness is the operating system's, through the keys of std's `RandomState`; the time,
/// the process id and a counter only add variety. It cannot fail or panic.
fn new_instance_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default()
        .hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    COUNTER.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn mdns_error(error: impl std::fmt::Display) -> DiscoveryError {
    DiscoveryError::Mdns(error.to_string())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    #[test]
    fn discovery_can_be_shared_between_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Discovery>();
    }

    #[test]
    fn instance_ids_are_16_lowercase_hex_characters_and_differ() {
        let ids: Vec<String> = (0..64).map(|_| new_instance_id()).collect();
        for id in &ids {
            assert_eq!(id.len(), 16, "{id}");
            assert!(
                id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                "{id}"
            );
        }
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), ids.len());
    }

    #[test]
    fn names_are_cut_on_a_character_boundary() {
        assert_eq!(cut_name("Desk"), "Desk");
        assert_eq!(cut_name(""), "");
        let exact = "a".repeat(MAX_NAME_BYTES);
        assert_eq!(cut_name(&exact), exact);
        assert_eq!(cut_name(&"a".repeat(100)), exact);
        // Two-byte characters: 31 fit in 63 bytes, the 32nd would straddle the limit.
        assert_eq!(cut_name(&"é".repeat(40)), "é".repeat(31));
        // Four-byte characters: 15 fit.
        assert_eq!(cut_name(&"🦀".repeat(40)), "🦀".repeat(15));
        // A multi-byte character starting at byte 62 is dropped whole.
        let name = format!("{}é", "a".repeat(62));
        assert_eq!(cut_name(&name), "a".repeat(62));
    }

    #[test]
    fn instance_labels_come_out_of_full_names() {
        let full = format!("0123456789abcdef.{SERVICE_TYPE}");
        assert_eq!(instance_of(&full), Some("0123456789abcdef"));
        assert_eq!(
            instance_of(&full.to_ascii_uppercase()),
            Some("0123456789ABCDEF")
        );
        assert_eq!(instance_of(SERVICE_TYPE), None);
        assert_eq!(instance_of(&format!(".{SERVICE_TYPE}")), None);
        assert_eq!(instance_of("0123456789abcdef._other._udp.local."), None);
        assert_eq!(instance_of(""), None);
        // Not a char boundary at the split point.
        assert_eq!(instance_of("é_crosspane._udp.local."), None);
    }

    #[test]
    fn our_own_instance_is_recognised_even_after_a_conflict_rename() {
        let own = "0123456789abcdef";
        assert!(is_own(own, "0123456789abcdef"));
        assert!(is_own(own, "0123456789ABCDEF"));
        assert!(is_own(own, "0123456789abcdef (2)"));
        assert!(!is_own(own, "0123456789abcdee"));
        assert!(!is_own(own, "0123456789abcdef0"));
        assert!(!is_own(own, "0123456789abcde"));
        assert!(!is_own(own, "0123456789abcdé"));
        assert!(!is_own(own, ""));
    }

    #[test]
    fn only_dialable_addresses_become_socket_addresses() {
        let scoped = |ip: IpAddr| ScopedIp::from(ip);
        assert_eq!(
            socket_addr(&scoped(Ipv4Addr::new(192, 168, 1, 7).into()), 4000),
            Some("192.168.1.7:4000".parse().unwrap())
        );
        assert_eq!(
            socket_addr(&scoped("2001:db8::1".parse().unwrap()), 4000),
            Some("[2001:db8::1]:4000".parse().unwrap())
        );
        for refused in [
            "127.0.0.1",
            "::1",
            "0.0.0.0",
            "::",
            "224.0.0.251",
            "ff02::fb",
        ] {
            assert_eq!(
                socket_addr(&scoped(refused.parse().unwrap()), 4000),
                None,
                "{refused}"
            );
        }
    }

    #[test]
    fn the_advertisement_is_a_random_name_and_the_version() {
        let info = advertisement("0123456789abcdef", 4000, None, true).unwrap();
        assert_eq!(
            info.get_fullname(),
            format!("0123456789abcdef.{SERVICE_TYPE}")
        );
        assert_eq!(info.get_hostname(), "0123456789abcdef.local.");
        assert!(info.is_addr_auto());
        assert_eq!(info.get_property_val_str("v"), Some("1"));
        assert!(info.get_property("n").is_none());
        let keys: Vec<&str> = info.get_properties().iter().map(|p| p.key()).collect();
        assert_eq!(keys, ["v"]);

        let named = advertisement("0123456789abcdef", 4000, Some(&"é".repeat(100)), false).unwrap();
        assert_eq!(
            named.get_property_val_str("n"),
            Some("é".repeat(31).as_str())
        );
        assert!(!named.requires_probe());
    }

    #[test]
    fn changes_wait_for_the_spacing() {
        let mut advert = Advert::default();
        assert!(!advert.due(), "nothing to send");
        advert.wanted = Some("Desk".into());
        assert!(advert.due(), "the first change is not held back");
        advert.sent = advert.wanted.clone();
        advert.last_change = Some(Instant::now());
        advert.wanted = None;
        assert!(!advert.due(), "too soon after the last change");
        advert.last_change = Some(Instant::now() - CHANGE_SPACING);
        assert!(advert.due());
        advert.wanted = advert.sent.clone();
        assert!(!advert.due(), "changed back before it was sent");
    }
}
