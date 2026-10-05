//! Read-only Windows interface snapshots and owned IP-change registrations.
//! Notifications perform one atomic signal only. Enumeration, delivery and cancellation
//! run outside their callbacks; callbacks never wait on a resource used by cancellation.
//! https://learn.microsoft.com/windows/win32/api/netioapi/nf-netioapi-cancelmibchangenotify2
#![allow(unsafe_code)]

use crate::model::link::{Adapter, Coalescer, normalize};
use crosspane_platform::{EventSink, Interface, LinkInfo, PlatformError};
use std::{
    collections::BTreeSet,
    ffi::c_void,
    mem::size_of,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::null,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows_sys::Win32::{Foundation::*, NetworkManagement::IpHelper::*, Networking::WinSock::*};

const BOUND: Duration = Duration::from_secs(2);
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_ADAPTERS: usize = 128;
const MAX_ADDRESSES: usize = 4096;

fn backend(message: &'static str) -> PlatformError {
    PlatformError::Backend(message.into())
}
fn elapsed(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[derive(Default)]
struct Signal {
    revision: AtomicU64,
}
impl Signal {
    fn changed(&self) {
        self.revision.fetch_add(1, Ordering::Release);
    }
}
#[derive(Default)]
struct DeliveryState {
    sink: Option<Arc<dyn EventSink<Vec<Interface>>>>,
    initial: Option<Vec<Interface>>,
    latest: Option<Vec<Interface>>,
    stopping: bool,
}
#[derive(Default)]
struct Delivery {
    state: Mutex<DeliveryState>,
    wake: Condvar,
}
impl Delivery {
    fn replace(
        &self,
        sink: Arc<dyn EventSink<Vec<Interface>>>,
        snapshot: Vec<Interface>,
    ) -> Result<(), PlatformError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| backend("link delivery poisoned"))?;
        if state.stopping {
            return Err(backend("link delivery stopped"));
        }
        state.sink = Some(sink);
        state.initial = Some(snapshot);
        state.latest = None;
        self.wake.notify_one();
        Ok(())
    }
    fn publish(&self, snapshot: Vec<Interface>) {
        if let Ok(mut state) = self.state.lock()
            && !state.stopping
            && state.sink.is_some()
        {
            state.latest = Some(snapshot);
            self.wake.notify_one();
        }
    }
    fn stop(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.stopping = true;
            state.sink = None;
            state.initial = None;
            state.latest = None;
            self.wake.notify_one();
        }
    }
    fn run(&self) {
        loop {
            let pending = {
                let Ok(mut state) = self.state.lock() else {
                    return;
                };
                loop {
                    if state.stopping {
                        return;
                    }
                    let snapshot = state.initial.take().or_else(|| state.latest.take());
                    if let (Some(snapshot), Some(sink)) = (snapshot, state.sink.as_ref()) {
                        break (Arc::clone(sink), snapshot);
                    }
                    let Ok(next) = self.wake.wait(state) else {
                        return;
                    };
                    state = next;
                }
            };
            if catch_unwind(AssertUnwindSafe(|| pending.0.send(pending.1))).is_err() {
                eprintln!("Windows link subscriber panicked");
            }
        }
    }
}
struct Request {
    until: Instant,
    abandoned: Arc<AtomicBool>,
    sink: Option<Arc<dyn EventSink<Vec<Interface>>>>,
    reply: mpsc::SyncSender<Result<Vec<Interface>, PlatformError>>,
}

/// Retain this backend for the subscription lifetime. A replacement subscribe sends a
/// new initial snapshot; an already-entered old sink may finish. No address/MAC logging.
pub struct WindowsLinkInfo {
    requests: mpsc::SyncSender<Request>,
    alive: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    fault: Arc<AtomicBool>,
    registered: Arc<AtomicU32>,
    cancelled: Arc<AtomicU32>,
    delivery: Arc<Delivery>,
    worker: Option<JoinHandle<()>>,
    dispatcher: Option<JoinHandle<()>>,
}
impl std::fmt::Debug for WindowsLinkInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsLinkInfo")
            .field("alive", &self.alive.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}
impl WindowsLinkInfo {
    pub fn new() -> Result<Self, PlatformError> {
        let (requests, incoming) = mpsc::sync_channel(8);
        let (ready, startup) = mpsc::sync_channel(1);
        let alive = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let fault = Arc::new(AtomicBool::new(false));
        let registered = Arc::new(AtomicU32::new(0));
        let cancelled = Arc::new(AtomicU32::new(0));
        let delivery = Arc::new(Delivery::default());
        let dispatch = Arc::clone(&delivery);
        let dispatcher = thread::Builder::new()
            .name("crosspane-links-delivery".into())
            .spawn(move || dispatch.run())
            .map_err(|_| backend("spawn link delivery failed"))?;
        let observing = Arc::clone(&alive);
        let stopping = Arc::clone(&stop);
        let failed = Arc::clone(&fault);
        let registrations = Arc::clone(&registered);
        let cancellations = Arc::clone(&cancelled);
        let delivered = Arc::clone(&delivery);
        let worker = match thread::Builder::new()
            .name("crosspane-links".into())
            .spawn(move || {
                let _exit = Exit {
                    alive: observing,
                    delivery: Arc::clone(&delivered),
                };
                match Notifications::new(failed, registrations, cancellations) {
                    Ok(notifications) => {
                        if ready.send(Ok(())).is_ok() {
                            observe(incoming, &stopping, &delivered, &notifications.signal);
                        }
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                }
            }) {
            Ok(worker) => worker,
            Err(_) => {
                delivery.stop();
                let _ = dispatcher.join();
                return Err(backend("spawn link observer failed"));
            }
        };
        let result = Self {
            requests,
            alive,
            stop,
            fault,
            registered,
            cancelled,
            delivery,
            worker: Some(worker),
            dispatcher: Some(dispatcher),
        };
        startup
            .recv_timeout(BOUND)
            .map_err(|_| PlatformError::Timeout)??;
        Ok(result)
    }
    fn request(
        &self,
        sink: Option<Arc<dyn EventSink<Vec<Interface>>>>,
    ) -> Result<Vec<Interface>, PlatformError> {
        if !self.alive.load(Ordering::Acquire) || self.stop.load(Ordering::Acquire) {
            return Err(backend("link observer unavailable"));
        }
        let until = Instant::now() + BOUND;
        let abandoned = Arc::new(AtomicBool::new(false));
        let (reply, result) = mpsc::sync_channel(1);
        self.requests
            .try_send(Request {
                until,
                abandoned: Arc::clone(&abandoned),
                sink,
                reply,
            })
            .map_err(|_| backend("link observer busy"))?;
        match result.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(result) => {
                if !self.alive.load(Ordering::Acquire) || self.stop.load(Ordering::Acquire) {
                    Err(backend("link observer unavailable"))
                } else {
                    result
                }
            }
            Err(_) => {
                abandoned.store(true, Ordering::Release);
                Err(PlatformError::Timeout)
            }
        }
    }
    fn stop_owned(&mut self) -> bool {
        self.stop.store(true, Ordering::Release);
        self.delivery.stop();
        let until = Instant::now() + BOUND;
        while Instant::now() < until
            && [&self.worker, &self.dispatcher]
                .into_iter()
                .any(|worker| worker.as_ref().is_some_and(|worker| !worker.is_finished()))
        {
            thread::sleep(Duration::from_millis(5));
        }
        let mut joined = true;
        for handle in [&mut self.worker, &mut self.dispatcher] {
            if handle.as_ref().is_some_and(|worker| worker.is_finished()) {
                if let Some(worker) = handle.take() {
                    joined &= worker.join().is_ok();
                }
            } else if handle.is_some() {
                joined = false;
            }
        }
        joined
            && !self.fault.load(Ordering::Acquire)
            && self.cancelled.load(Ordering::Acquire) == self.registered.load(Ordering::Acquire)
    }
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn stop_verified(mut self) -> bool {
        self.stop_owned()
    }
}
impl LinkInfo for WindowsLinkInfo {
    fn interfaces(&self) -> Result<Vec<Interface>, PlatformError> {
        self.request(None)
    }
    fn subscribe(&mut self, sink: Arc<dyn EventSink<Vec<Interface>>>) -> Result<(), PlatformError> {
        self.request(Some(sink)).map(|_| ())
    }
}
impl Drop for WindowsLinkInfo {
    fn drop(&mut self) {
        if !self.stop_owned() {
            eprintln!("Windows link cleanup unverified; own workers/callback resources retained");
        }
    }
}
struct Exit {
    alive: Arc<AtomicBool>,
    delivery: Arc<Delivery>,
}
impl Drop for Exit {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        self.delivery.publish(Vec::new());
    }
}
fn accept_snapshot(
    snapshot: &[Interface],
    sink: Option<Arc<dyn EventSink<Vec<Interface>>>>,
    delivery: &Delivery,
    coalescer: &mut Coalescer,
) -> Result<(), PlatformError> {
    if let Some(sink) = sink {
        let initial = coalescer.initial(snapshot.to_vec());
        delivery.replace(sink, initial)?;
    }
    Ok(())
}
fn observe(
    incoming: mpsc::Receiver<Request>,
    stop: &AtomicBool,
    delivery: &Delivery,
    signal: &Signal,
) {
    let start = Instant::now();
    let mut revision = signal.revision.load(Ordering::Acquire);
    let mut coalescer = Coalescer::default();
    while !stop.load(Ordering::Acquire) {
        match incoming.recv_timeout(Duration::from_millis(10)) {
            Ok(request) => {
                if request.abandoned.load(Ordering::Acquire) || Instant::now() >= request.until {
                    continue;
                }
                let mut result = snapshot(request.until);
                if request.abandoned.load(Ordering::Acquire)
                    || stop.load(Ordering::Acquire)
                    || Instant::now() >= request.until
                {
                    continue;
                }
                if let Ok(snapshot) = &result
                    && let Err(error) =
                        accept_snapshot(snapshot, request.sink, delivery, &mut coalescer)
                {
                    result = Err(error);
                }
                let _ = request.reply.try_send(result);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let current = signal.revision.load(Ordering::Acquire);
        if current != revision {
            revision = current;
            coalescer.signal(elapsed(start));
        }
        if coalescer.due(elapsed(start)) {
            let result = snapshot(Instant::now() + BOUND);
            if stop.load(Ordering::Acquire) {
                break;
            }
            if let Some(changed) =
                coalescer.observed(result.as_ref().ok().cloned().unwrap_or_default())
            {
                delivery.publish(changed);
            }
            if result.is_err() {
                coalescer.signal(elapsed(start));
            }
        }
    }
}
struct Notifications {
    signal: Arc<Signal>,
    interface: HANDLE,
    unicast: HANDLE,
    fault: Arc<AtomicBool>,
    registered: Arc<AtomicU32>,
    cancelled: Arc<AtomicU32>,
    cancel: fn(HANDLE) -> u32,
}
impl Notifications {
    fn new(
        fault: Arc<AtomicBool>,
        registered: Arc<AtomicU32>,
        cancelled: Arc<AtomicU32>,
    ) -> Result<Self, PlatformError> {
        Self::register(fault, registered, cancelled, register_native, cancel_native)
    }
    fn register(
        fault: Arc<AtomicBool>,
        registered: Arc<AtomicU32>,
        cancelled: Arc<AtomicU32>,
        mut register: impl FnMut(bool, *const c_void, *mut HANDLE) -> u32,
        cancel: fn(HANDLE) -> u32,
    ) -> Result<Self, PlatformError> {
        let mut owned = Self {
            signal: Arc::new(Signal::default()),
            interface: std::ptr::null_mut(),
            unicast: std::ptr::null_mut(),
            fault,
            registered,
            cancelled,
            cancel,
        };
        if register(
            false,
            Arc::as_ptr(&owned.signal).cast(),
            &mut owned.interface,
        ) != 0
        {
            return Err(backend("register interface notifications failed"));
        }
        owned.registered.fetch_add(1, Ordering::Release);
        if register(true, Arc::as_ptr(&owned.signal).cast(), &mut owned.unicast) != 0 {
            return Err(backend("register address notifications failed"));
        }
        owned.registered.fetch_add(1, Ordering::Release);
        Ok(owned)
    }
}
impl Drop for Notifications {
    fn drop(&mut self) {
        for handle in [self.interface, self.unicast] {
            if handle.is_null() {
                continue;
            }
            if (self.cancel)(handle) == 0 {
                self.cancelled.fetch_add(1, Ordering::Release);
            } else {
                self.fault.store(true, Ordering::Release);
                // Failed cancellation cannot free memory still referenced by an OS callback.
                std::mem::forget(Arc::clone(&self.signal));
            }
        }
    }
}
fn register_native(unicast: bool, context: *const c_void, handle: *mut HANDLE) -> u32 {
    // SAFETY: live owned Arc context, static callbacks, initialized handle output; no initial
    // callback enumeration. The caller retains context through completed cancellation.
    unsafe {
        if unicast {
            NotifyUnicastIpAddressChange(AF_UNSPEC, Some(unicast_changed), context, false, handle)
        } else {
            NotifyIpInterfaceChange(AF_UNSPEC, Some(interface_changed), context, false, handle)
        }
    }
}
fn cancel_native(handle: HANDLE) -> u32 {
    // SAFETY: only our returned registration, outside its callback. The callback takes
    // no locks and waits on no resource owned by this cancellation thread.
    unsafe { CancelMibChangeNotify2(handle) }
}

// SAFETY: IP Helper receives the static function and our live retained Signal context.
unsafe extern "system" fn interface_changed(
    context: *const c_void,
    _: *const MIB_IPINTERFACE_ROW,
    _: MIB_NOTIFICATION_TYPE,
) {
    // SAFETY: exact registered context; callback performs a single O(1) atomic signal.
    unsafe {
        (&*context.cast::<Signal>()).changed();
    }
}
// SAFETY: IP Helper receives the static function and our live retained Signal context.
unsafe extern "system" fn unicast_changed(
    context: *const c_void,
    _: *const MIB_UNICASTIPADDRESS_ROW,
    _: MIB_NOTIFICATION_TYPE,
) {
    // SAFETY: exact registered context; never reads OS callback rows or allocates/enumerates.
    unsafe {
        (&*context.cast::<Signal>()).changed();
    }
}

/// u64 backing gives the adapter's required eight-byte alignment. All linked objects
/// and UTF16 reads are checked against this owned initialized allocation before access.
struct Buffer {
    words: Vec<u64>,
}
impl Buffer {
    fn new(bytes: usize) -> Result<Self, PlatformError> {
        if bytes == 0 || bytes > MAX_BYTES {
            return Err(backend("adapter buffer exceeds bound"));
        }
        let count = bytes.div_ceil(size_of::<u64>());
        let mut words = Vec::new();
        words
            .try_reserve_exact(count)
            .map_err(|_| backend("allocate adapter buffer failed"))?;
        words.resize(count, 0);
        Ok(Self { words })
    }
    fn bytes(&self) -> usize {
        self.words.len() * size_of::<u64>()
    }
    fn contains(&self, pointer: usize, bytes: usize) -> bool {
        let start = self.words.as_ptr() as usize;
        pointer >= start
            && pointer
                .checked_add(bytes)
                .is_some_and(|end| end <= start + self.bytes())
    }
    fn read<T: Copy>(&self, pointer: *const T) -> Result<T, PlatformError> {
        if !self.contains(pointer as usize, size_of::<T>()) {
            return Err(backend("adapter pointer outside buffer"));
        }
        // SAFETY: initialized OS output inside the owned allocation, proven above. Reading
        // unaligned handles packed socket/string nodes without inventing alignment.
        Ok(unsafe { pointer.read_unaligned() })
    }
    fn string(&self, pointer: *const u16) -> Result<String, PlatformError> {
        let mut chars = Vec::new();
        for index in 0..1024 {
            let address = (pointer as usize)
                .checked_add(index * 2)
                .ok_or_else(|| backend("adapter string overflow"))?;
            let ch: u16 = self.read(address as *const u16)?;
            if ch == 0 {
                return String::from_utf16(&chars)
                    .map_err(|_| backend("invalid adapter name encoding"));
            }
            chars.push(ch);
        }
        Err(backend("adapter name exceeds bound"))
    }
}
fn snapshot(until: Instant) -> Result<Vec<Interface>, PlatformError> {
    let mut bytes = 15_000;
    for _ in 0..3 {
        if Instant::now() >= until {
            return Err(PlatformError::Timeout);
        }
        let mut buffer = Buffer::new(bytes)?;
        let mut length = buffer.bytes() as u32;
        // SAFETY: owned aligned initialized buffer and exact capacity; AF_UNSPEC retrieves
        // both families, skipping unrelated lists. Public read-only API; no configuration.
        let result = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_UNSPEC),
                GAA_FLAG_SKIP_ANYCAST
                    | GAA_FLAG_SKIP_MULTICAST
                    | GAA_FLAG_SKIP_DNS_SERVER
                    | GAA_FLAG_INCLUDE_ALL_INTERFACES,
                null(),
                buffer.words.as_mut_ptr().cast(),
                &mut length,
            )
        };
        if Instant::now() >= until {
            return Err(PlatformError::Timeout);
        }
        if result == ERROR_BUFFER_OVERFLOW {
            bytes = length as usize;
            continue;
        }
        if result == ERROR_NO_DATA {
            return Ok(Vec::new());
        }
        if result != ERROR_SUCCESS || length as usize > buffer.bytes() {
            return Err(backend("enumerate adapter addresses failed"));
        }
        let normalized = normalize(parse(&buffer, until)?);
        if normalized.scope_mismatches != 0 {
            eprintln!(
                "Windows link IPv6 scope/index mismatch count={}",
                normalized.scope_mismatches
            );
        }
        return Ok(normalized.interfaces);
    }
    Err(PlatformError::Timeout)
}
fn parse(buffer: &Buffer, until: Instant) -> Result<Vec<Adapter>, PlatformError> {
    let mut pointer = buffer.words.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    let mut visited = BTreeSet::new();
    let mut total = 0;
    let mut adapters = Vec::new();
    while !pointer.is_null() {
        if Instant::now() >= until {
            return Err(PlatformError::Timeout);
        }
        if visited.len() >= MAX_ADAPTERS || !visited.insert(pointer as usize) {
            return Err(backend("adapter list exceeds bound"));
        }
        let adapter = buffer.read(pointer)?;
        // SAFETY: OS initializes the anonymous Length/IfIndex structure of this checked node.
        let head = unsafe { adapter.Anonymous1.Anonymous };
        if (head.Length as usize) < size_of::<IP_ADAPTER_ADDRESSES_LH>() {
            return Err(backend("unsupported adapter structure"));
        }
        let (physical_medium, virtual_hint) = metadata(&adapter);
        adapters.push(Adapter {
            name: buffer.string(adapter.FriendlyName)?,
            description: buffer.string(adapter.Description)?,
            index_v4: head.IfIndex,
            index_v6: adapter.Ipv6IfIndex,
            if_type: adapter.IfType,
            physical_medium,
            virtual_hint,
            tunnel_type: adapter.TunnelType as u32,
            oper_status: adapter.OperStatus as u32,
            mtu: adapter.Mtu,
            receive_bps: adapter.ReceiveLinkSpeed,
            transmit_bps: adapter.TransmitLinkSpeed,
            addrs: addresses(buffer, adapter.FirstUnicastAddress, &mut total)?,
        });
        pointer = adapter.Next;
    }
    Ok(adapters)
}
fn metadata(adapter: &IP_ADAPTER_ADDRESSES_LH) -> (Option<u32>, Option<bool>) {
    let mut row = MIB_IF_ROW2 {
        InterfaceLuid: adapter.Luid,
        ..Default::default()
    };
    // SAFETY: exact initialized read-only row keyed by the observed LUID; outputs remain local.
    let result = unsafe { GetIfEntry2(&mut row) };
    // SAFETY: documented initialized LUID value union in both observations.
    let same_luid = unsafe { row.InterfaceLuid.Value == adapter.Luid.Value };
    if result != 0
        || !same_luid
        || row.Type != adapter.IfType
        || row.TunnelType != adapter.TunnelType
    {
        return (None, None);
    }
    (
        u32::try_from(row.PhysicalMediumType).ok(),
        Some(row.InterfaceAndOperStatusFlags._bitfield & 1 == 0),
    )
}
fn addresses(
    buffer: &Buffer,
    mut pointer: *const IP_ADAPTER_UNICAST_ADDRESS_LH,
    total: &mut usize,
) -> Result<Vec<(IpAddr, u32)>, PlatformError> {
    let mut visited = BTreeSet::new();
    let mut result = Vec::new();
    while !pointer.is_null() {
        if *total >= MAX_ADDRESSES || !visited.insert(pointer as usize) {
            return Err(backend("unicast list exceeds bound"));
        }
        *total += 1;
        let node = buffer.read(pointer)?;
        // SAFETY: documented initialized Length/Flags variant in this checked unicast node.
        if (unsafe { node.Anonymous.Anonymous.Length } as usize)
            < size_of::<IP_ADAPTER_UNICAST_ADDRESS_LH>()
        {
            return Err(backend("unsupported unicast structure"));
        }
        let socket = node.Address;
        if socket.iSockaddrLength < size_of::<ADDRESS_FAMILY>() as i32 {
            return Err(backend("truncated unicast socket"));
        }
        let family: ADDRESS_FAMILY = buffer.read(socket.lpSockaddr.cast())?;
        match family {
            AF_INET => {
                if socket.iSockaddrLength < size_of::<SOCKADDR_IN>() as i32 {
                    return Err(backend("truncated IPv4 socket"));
                }
                let socket: SOCKADDR_IN = buffer.read(socket.lpSockaddr.cast())?;
                // SAFETY: initialized IN_ADDR's raw four network-order bytes; no text parsing.
                let bytes = unsafe { socket.sin_addr.S_un.S_addr }.to_ne_bytes();
                result.push((IpAddr::V4(Ipv4Addr::from(bytes)), 0));
            }
            AF_INET6 => {
                if socket.iSockaddrLength < size_of::<SOCKADDR_IN6>() as i32 {
                    return Err(backend("truncated IPv6 socket"));
                }
                let socket: SOCKADDR_IN6 = buffer.read(socket.lpSockaddr.cast())?;
                // SAFETY: initialized documented address bytes/scope union from this checked node.
                let (bytes, scope) =
                    unsafe { (socket.sin6_addr.u.Byte, socket.Anonymous.sin6_scope_id) };
                result.push((IpAddr::V6(Ipv6Addr::from(bytes)), scope));
            }
            _ => return Err(backend("unexpected unicast address family")),
        }
        pointer = node.Next;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosspane_platform::LinkClass;

    fn sample(up: bool) -> Vec<Interface> {
        vec![Interface {
            name: "fixture".into(),
            index: 7,
            class: LinkClass::Unknown,
            up,
            mtu: None,
            speed_mbps: None,
            addrs: Vec::new(),
        }]
    }
    #[test]
    fn ordinary_snapshot_cannot_bypass_notification_debounce() {
        let delivery = Delivery::default();
        let mut coalescer = Coalescer::default();
        let first = coalescer.initial(sample(true));
        delivery.replace(Arc::new(|_| {}), first).unwrap();
        delivery.state.lock().unwrap().initial.take();
        coalescer.signal(100);
        accept_snapshot(&sample(false), None, &delivery, &mut coalescer).unwrap();
        assert!(delivery.state.lock().unwrap().latest.is_none());
        assert!(!coalescer.due(349));
        assert!(coalescer.due(350));
    }
    #[test]
    fn callback_signal_only_accepts_null_rows_without_enumeration() {
        let signal = Arc::new(Signal::default());
        // SAFETY: owned retained context; callbacks never inspect the deliberately null row.
        unsafe {
            interface_changed(Arc::as_ptr(&signal).cast(), null(), 0);
            unicast_changed(Arc::as_ptr(&signal).cast(), null(), 0);
        }
        assert_eq!(signal.revision.load(Ordering::Acquire), 2);
    }
    #[test]
    fn replacing_sink_drops_pending_old_delivery_and_keeps_initial_first() {
        let delivery = Delivery::default();
        let old = Arc::new(|_: Vec<Interface>| {});
        let weak = Arc::downgrade(&old);
        delivery.replace(old.clone(), sample(true)).unwrap();
        drop(old);
        delivery.publish(sample(false));
        delivery.replace(Arc::new(|_| {}), sample(true)).unwrap();
        assert!(weak.upgrade().is_none());
        let state = delivery.state.lock().unwrap();
        assert!(
            state
                .initial
                .as_ref()
                .is_some_and(|snapshot| snapshot[0].up)
        );
        assert!(state.latest.is_none());
        drop(state);
        delivery.stop();
        assert!(delivery.replace(Arc::new(|_| {}), sample(true)).is_err());
    }
    fn fake_cancel(handle: HANDLE) -> u32 {
        assert_eq!(handle as usize, 11);
        0
    }
    #[test]
    fn failed_second_registration_cancels_only_the_first_own_handle() {
        let registered = Arc::new(AtomicU32::new(0));
        let cancelled = Arc::new(AtomicU32::new(0));
        let result = Notifications::register(
            Arc::new(AtomicBool::new(false)),
            Arc::clone(&registered),
            Arc::clone(&cancelled),
            |unicast, _, handle| {
                if unicast {
                    return 5;
                }
                // SAFETY: fake-only output points at the production guard's initialized HANDLE.
                unsafe {
                    *handle = 11usize as HANDLE;
                }
                0
            },
            fake_cancel,
        );
        assert!(result.is_err());
        assert_eq!(registered.load(Ordering::Acquire), 1);
        assert_eq!(cancelled.load(Ordering::Acquire), 1);
    }
    #[test]
    fn successful_cancel_releases_context_but_failed_cancel_retains_it() {
        for (cancel, fails) in [
            ((|_: HANDLE| 0) as fn(HANDLE) -> u32, false),
            ((|_: HANDLE| 5) as fn(HANDLE) -> u32, true),
        ] {
            let fault = Arc::new(AtomicBool::new(false));
            let registered = Arc::new(AtomicU32::new(0));
            let cancelled = Arc::new(AtomicU32::new(0));
            let owned = Notifications::register(
                Arc::clone(&fault),
                Arc::clone(&registered),
                Arc::clone(&cancelled),
                |unicast, _, handle| {
                    // SAFETY: fake-only initialized output; these values never reach an OS call.
                    unsafe { *handle = if unicast { 22usize } else { 11usize } as HANDLE };
                    0
                },
                cancel,
            )
            .unwrap();
            let weak = Arc::downgrade(&owned.signal);
            drop(owned);
            assert_eq!(registered.load(Ordering::Acquire), 2);
            assert_eq!(cancelled.load(Ordering::Acquire), if fails { 0 } else { 2 });
            assert_eq!(fault.load(Ordering::Acquire), fails);
            assert_eq!(weak.upgrade().is_some(), fails);
        }
    }
    #[test]
    fn delivery_callback_is_reentrant_and_stop_joins_without_lock_cycle() {
        let delivery = Arc::new(Delivery::default());
        let stopping = Arc::clone(&delivery);
        let (sent, received) = mpsc::sync_channel(1);
        delivery
            .replace(
                Arc::new(move |_: Vec<Interface>| {
                    stopping.stop();
                    sent.send(()).unwrap();
                }),
                sample(true),
            )
            .unwrap();
        let running = Arc::clone(&delivery);
        let worker = thread::spawn(move || running.run());
        received.recv_timeout(BOUND).unwrap();
        worker.join().unwrap();
        assert!(delivery.state.lock().unwrap().sink.is_none());
    }
    #[test]
    fn completed_request_refuses_after_observer_death() {
        let (requests, incoming) = mpsc::sync_channel::<Request>(1);
        let alive = Arc::new(AtomicBool::new(true));
        let dying = Arc::clone(&alive);
        let worker = thread::spawn(move || {
            let request = incoming.recv_timeout(BOUND).unwrap();
            dying.store(false, Ordering::Release);
            request.reply.send(Ok(sample(true))).unwrap();
        });
        let backend = WindowsLinkInfo {
            requests,
            alive,
            stop: Arc::new(AtomicBool::new(false)),
            fault: Arc::new(AtomicBool::new(false)),
            registered: Arc::new(AtomicU32::new(0)),
            cancelled: Arc::new(AtomicU32::new(0)),
            delivery: Arc::new(Delivery::default()),
            worker: Some(worker),
            dispatcher: None,
        };
        assert!(matches!(
            backend.interfaces(),
            Err(PlatformError::Backend(_))
        ));
        assert!(backend.stop_verified());
    }
    #[test]
    fn owned_buffer_refuses_null_foreign_truncated_and_unterminated_reads() {
        let buffer = Buffer::new(16).unwrap();
        assert!(buffer.read::<u64>(null()).is_err());
        assert!(buffer.read::<u64>(usize::MAX as *const u64).is_err());
        assert!(
            buffer
                .read::<u64>((buffer.words.as_ptr() as usize + 12) as *const u64)
                .is_err()
        );
        let mut buffer = Buffer::new(16).unwrap();
        buffer.words.fill(u64::MAX);
        assert!(buffer.string(buffer.words.as_ptr().cast()).is_err());
        assert!(Buffer::new(MAX_BYTES + 1).is_err());
    }
    #[test]
    fn cyclic_and_truncated_unicast_fixtures_are_bounded() {
        let mut buffer = Buffer::new(256).unwrap();
        let pointer = buffer
            .words
            .as_mut_ptr()
            .cast::<IP_ADAPTER_UNICAST_ADDRESS_LH>();
        let socket = (buffer.words.as_mut_ptr() as usize + 128) as *mut SOCKADDR_IN;
        let node = IP_ADAPTER_UNICAST_ADDRESS_LH {
            Anonymous: IP_ADAPTER_UNICAST_ADDRESS_LH_0 {
                Anonymous: IP_ADAPTER_UNICAST_ADDRESS_LH_0_0 {
                    Length: size_of::<IP_ADAPTER_UNICAST_ADDRESS_LH>() as u32,
                    Flags: 0,
                },
            },
            Next: pointer,
            Address: SOCKET_ADDRESS {
                lpSockaddr: socket.cast(),
                iSockaddrLength: size_of::<SOCKADDR_IN>() as i32,
            },
            ..Default::default()
        };
        // SAFETY: initialized fake structs fit in this owned 256-byte allocation; no native API.
        unsafe {
            pointer.write(node);
            socket.write(SOCKADDR_IN {
                sin_family: AF_INET,
                ..Default::default()
            });
        }
        let mut total = 0;
        assert!(addresses(&buffer, pointer, &mut total).is_err());
        assert_eq!(total, 1);
        let mut node = node;
        node.Next = std::ptr::null_mut();
        node.Address.iSockaddrLength = 1;
        // SAFETY: rewrite only the own fake node at the same validated allocation address.
        unsafe {
            pointer.write(node);
        }
        assert!(addresses(&buffer, pointer, &mut 0).is_err());
    }
}
