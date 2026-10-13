//! Same-adapter DX12-born resources opened on WGC's D3D11 device.
//! No keyed mutex, picker, privilege change, or native-runtime claim. W6.2 verifies this path.
#![allow(unsafe_code)]

use crate::model::gpu::{self as model, Handoff, Reason, Slot};
use crosspane_platform::{IoGate, PlatformError};
use crosspane_types::geom::PixelSize;
use std::{
    fmt,
    future::Future,
    pin::pin,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
    thread,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, GENERIC_ALL, HANDLE, LUID},
        Graphics::{
            Direct3D11::*,
            Direct3D12::*,
            Dxgi::{Common::*, IDXGIDevice},
        },
    },
    core::Interface,
};
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_PIN, GetModuleHandleExW,
};

pub(crate) const BOUND: Duration = Duration::from_secs(2);

/// Keeps `d3d12.dll` loaded for the rest of the process.
///
/// wgpu-hal loads `d3d12.dll` with `LoadLibrary` and calls `FreeLibrary` when the last of its
/// instance, adapter and device handles goes. Every D3D12 interface this module clones out of
/// wgpu or creates itself is released through that DLL: `ID3D12Device::Release` reads
/// `d3d12.dll` data, so a release after the unload faults with STATUS_ACCESS_VIOLATION inside
/// `D3D12Core.dll` (reproduced by `tests/gpu_drop.rs`: the faulting read was inside the unloaded
/// `d3d12.dll` image). `WindowsGpu` orders its own fields so that cannot happen, but resources
/// it hands out (`SharedTexture`, `Fence`, encoder input slots, a `Lease`) may be released by
/// code that does not control that order, so the library is pinned: it is a system DLL this
/// process keeps using until it exits anyway. Pinning never loads anything; it only marks the
/// already loaded module as unremovable, so it must run after wgpu created its DX12 instance.
fn pin_d3d12_runtime() -> Result<(), PlatformError> {
    static PINNED: AtomicBool = AtomicBool::new(false);
    if PINNED.load(Ordering::Acquire) {
        return Ok(());
    }
    let name: Vec<u16> = "d3d12.dll".encode_utf16().chain(Some(0)).collect();
    let mut module = std::ptr::null_mut();
    // SAFETY: NUL-terminated wide module name and a valid output slot. FLAG_PIN only marks an
    // already loaded module as unremovable; it neither loads one nor takes another call's pin.
    let pinned =
        unsafe { GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_PIN, name.as_ptr(), &mut module) };
    if pinned == 0 {
        // No DX12 runtime in this process: there is nothing for a GPU context to share with.
        return Err(error(Reason::Adapter));
    }
    PINNED.store(true, Ordering::Release);
    Ok(())
}
pub(crate) fn error(reason: Reason) -> PlatformError {
    // The node locking, unlocking or retiring this capture is a lifecycle event, not a GPU
    // failure. Callers already treat `Locked` as "stop quietly, keep the device".
    if reason.lifecycle() {
        PlatformError::Locked
    } else {
        PlatformError::Backend(reason.name().into())
    }
}
/// Milliseconds on a process-local monotonic clock, for the retirement/rebuild bookkeeping.
pub(crate) fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}
pub(crate) fn api<T>(value: windows::core::Result<T>) -> Result<T, PlatformError> {
    value.map_err(|e| PlatformError::Backend(format!("GPU HRESULT {:08x}", e.code().0 as u32)))
}
/// As `api`, naming the call that failed so a refusal can be traced to one step.
fn step<T>(call: &str, value: windows::core::Result<T>) -> Result<T, PlatformError> {
    value.map_err(|e| {
        PlatformError::Backend(format!("GPU {call} HRESULT {:08x}", e.code().0 as u32))
    })
}
fn luid(value: LUID) -> u64 {
    (value.HighPart as u32 as u64) << 32 | u64::from(value.LowPart)
}

struct WakeThread(thread::Thread);
impl Wake for WakeThread {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}
fn wait_future<T>(future: impl Future<Output = T>, until: Instant) -> Result<T, PlatformError> {
    let waker = Waker::from(Arc::new(WakeThread(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if Instant::now() >= until {
            return Err(PlatformError::Timeout);
        }
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return Ok(value);
        }
        thread::park_timeout(Duration::from_millis(1));
    }
}

/// Source-only context. The proxy host owns a different device and queue.
///
/// Fields drop in declaration order, after `Drop::drop` has run, and that order is load-bearing:
/// 1. `retained` holds D3D12 objects whose GPU work may still be running. `Drop::drop` empties it
///    first (waiting for the work, or abandoning it on a retired instance), and it is declared
///    first so that nothing it owned can outlive the rest.
/// 2. `raw_queue` and `d12` are our own references to the DX12 queue and device that live inside
///    wgpu. They are released while the wgpu device, and the `d3d12.dll` handle that device
///    carries, are still alive: releasing them after wgpu-hal's `FreeLibrary` is the access
///    violation this order exists to prevent. (The library is also pinned, see
///    `pin_d3d12_runtime`, for native objects that outlive this struct.)
/// 3. `queue` then `device` are wgpu's handles; dropping the last of them tears down the hal
///    device.
/// 4. `context` and `d11` are the capture backend's D3D11 device, shared with it, last.
///
/// Holders of this context must likewise declare their `Arc<WindowsGpu>` *after* every native
/// D3D12/D3D11 object they own (see `Surface`).
pub struct WindowsGpu {
    retained: Mutex<Vec<Retained>>,
    pub(crate) raw_queue: ID3D12CommandQueue,
    pub(crate) d12: ID3D12Device,
    pub queue: wgpu::Queue,
    pub device: wgpu::Device,
    pub(crate) context: Arc<Mutex<ID3D11DeviceContext>>,
    pub(crate) d11: ID3D11Device,
    pub(crate) luid: u64,
    pub(crate) video_supported: bool,
    wanted: wgpu::Features,
    serial: Mutex<()>,
    next: AtomicU64,
    latch: Mutex<model::Latch>,
    refusal: Mutex<Option<String>>,
}
impl Drop for WindowsGpu {
    fn drop(&mut self) {
        // Nobody can submit work any more (no `Arc` is left), so what remains is the deferred
        // release of resources a finished surface left behind. A healthy instance waits for the
        // GPU to finish with them. A retired one (an unconfirmed handoff) has a fence that may
        // never pass, so it gets one look and then abandons what is left: a leaked resource is
        // harmless, a resource released under running GPU work is not. A removed device reports
        // `u64::MAX` and is released at once.
        let patient = self.retired().is_none();
        let until = Instant::now() + if patient { BOUND } else { Duration::ZERO };
        let retained = self
            .retained
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for held in std::mem::take(retained) {
            loop {
                // SAFETY: read-only completion query on a fence this entry still owns.
                let completed = unsafe { held.fence.GetCompletedValue() };
                // `u64::MAX` is a removed device: nothing is running, release is safe.
                if completed == u64::MAX || completed >= held.value {
                    drop(held);
                    break;
                }
                if Instant::now() >= until {
                    // Unclean ownership is never a safe early free.
                    std::mem::forget(held);
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
}
impl fmt::Debug for WindowsGpu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowsGpu")
            .field("luid", &self.luid)
            .finish_non_exhaustive()
    }
}
impl WindowsGpu {
    pub(crate) fn new(
        d11: ID3D11Device,
        context: Arc<Mutex<ID3D11DeviceContext>>,
        wanted: wgpu::Features,
    ) -> Result<Arc<Self>, PlatformError> {
        let until = Instant::now() + BOUND;
        let dxgi: IDXGIDevice = api(d11.cast())?;
        // SAFETY: read-only adapter metadata from our retained capture device.
        let capture = unsafe { api(api(dxgi.GetAdapter())?.GetDesc())? };
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::DX12;
        let instance = wgpu::Instance::new(desc);
        // After the instance (which loads d3d12.dll) and before any D3D12 object of ours exists.
        pin_d3d12_runtime()?;
        let adapters = wait_future(instance.enumerate_adapters(wgpu::Backends::DX12), until)?;
        let adapter = adapters
            .into_iter()
            .find(|adapter| {
                // SAFETY: guard only queries the owned DX12 adapter; no raw object is destroyed.
                unsafe { adapter.as_hal::<wgpu::hal::api::Dx12>() }.is_some_and(|hal| {
                    // SAFETY: metadata query on the guarded live adapter.
                    unsafe { hal.raw_adapter().GetDesc() }
                        .is_ok_and(|d| luid(d.AdapterLuid) == luid(capture.AdapterLuid))
                })
            })
            .ok_or_else(|| error(Reason::Adapter))?;
        let (device, queue) = wait_future(
            adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("crosspane Windows source"),
                required_features: wanted & adapter.features(),
                ..Default::default()
            }),
            until,
        )?
        .map_err(|_| error(Reason::Sharing))?;
        // SAFETY: clone owned native references; HAL guards are dropped before wgpu submissions.
        let d12 = unsafe { device.as_hal::<wgpu::hal::api::Dx12>() }
            .ok_or_else(|| error(Reason::Adapter))?
            .raw_device()
            .clone();
        // SAFETY: this is the submission queue, not Device.raw_queue's presentation queue.
        let raw_queue = unsafe { queue.as_hal::<wgpu::hal::api::Dx12>() }
            .ok_or_else(|| error(Reason::Adapter))?
            .as_raw()
            .clone();
        // SAFETY: reads our device's immutable creation flags, never another process's device.
        let video_supported =
            unsafe { d11.GetCreationFlags() } & D3D11_CREATE_DEVICE_VIDEO_SUPPORT.0 != 0;
        // SAFETY: verifies actual device identity and enables public immediate-context protection.
        unsafe {
            model::admit(
                Some(luid(capture.AdapterLuid)),
                Some(luid(d12.GetAdapterLuid())),
                false,
            )
            .map_err(error)?;
            let protected: ID3D11Multithread =
                api(context.lock().map_err(|_| error(Reason::Sharing))?.cast())?;
            // Set returns the previous protection state, rather than a success value.
            let _ = protected.SetMultithreadProtected(true);
            if !protected.GetMultithreadProtected().as_bool() {
                return Err(error(Reason::Sharing));
            }
        }
        let result = Arc::new(Self {
            retained: Mutex::new(Vec::new()),
            raw_queue,
            d12,
            queue,
            device,
            context,
            d11,
            luid: luid(capture.AdapterLuid),
            video_supported,
            wanted,
            serial: Mutex::new(()),
            next: AtomicU64::new(1),
            latch: Mutex::new(model::Latch::default()),
            refusal: Mutex::new(None),
        });
        result.healthy()?;
        Ok(result)
    }
    pub(crate) fn healthy(&self) -> Result<(), PlatformError> {
        if let Ok(mut retained) = self.retained.lock() {
            retained.retain(|held| {
                // SAFETY: owned native resources remain retained until completion or removal.
                let value = unsafe { held.fence.GetCompletedValue() };
                value != u64::MAX && value < held.value
            });
        }
        if let Err(reason) = self.latch().healthy() {
            return Err(error(reason));
        }
        // SAFETY: read-only removal status of our retained devices, not an owner session query.
        let removed = unsafe {
            api(self.d11.GetDeviceRemovedReason())
                .and_then(|()| api(self.d12.GetDeviceRemovedReason()))
        };
        if removed.is_err() {
            // A removed or reset device never comes back; a rebuild makes a new one.
            self.retire_for(model::Fault::Device, Reason::Removed);
        }
        removed
    }
    fn latch(&self) -> MutexGuard<'_, model::Latch> {
        // Plain data with no invariant spanning a panic: a poisoned guard is still correct.
        self.latch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    /// Retires this instance after an unconfirmed handoff. The instance never serves again;
    /// the capture side may build a fresh one later.
    pub(crate) fn retire(&self) {
        self.retire_for(model::Fault::Handoff, Reason::Submitted);
    }
    pub(crate) fn retire_for(&self, fault: model::Fault, reason: Reason) {
        self.latch().retire(fault, reason, now_ms());
    }
    /// Why this instance is retired, if it is.
    pub(crate) fn retired(&self) -> Option<model::Retired> {
        self.latch().retired()
    }
    /// The features this instance was created with, for building its replacement.
    pub(crate) fn wanted(&self) -> wgpu::Features {
        self.wanted
    }
    /// One handoff of a surface timed out. Isolated timeouts cost only that surface; several in
    /// a row mean the queue is stalled and retire the instance.
    pub(crate) fn strike(&self) {
        let mut latch = self.latch();
        let _ = latch.strike(now_ms());
    }
    pub(crate) fn settled(&self) {
        self.latch().settled();
    }
    /// A slot could not be created. Remembers why; persistent failure latches `Unsupported`.
    pub(crate) fn surface_failed(&self, cause: &PlatformError) {
        self.note_refusal(cause);
        // A size this adapter cannot hold says nothing about sharing; a retired instance
        // already has its cause.
        let size_only =
            matches!(cause, PlatformError::Backend(name) if name == Reason::Layout.name());
        if !size_only && self.latch().retired().is_none() {
            let _ = self.latch().surface_failed(now_ms());
        }
    }
    /// Remembers the last refusal for `off_reason` without counting it against the instance.
    pub(crate) fn note_refusal(&self, cause: &PlatformError) {
        if let Ok(mut refusal) = self.refusal.lock() {
            *refusal = Some(format!("{cause:?}"));
        }
    }
    pub(crate) fn surface_created(&self) {
        self.latch().surface_created();
        if let Ok(mut refusal) = self.refusal.lock() {
            *refusal = None;
        }
    }
    /// Why the GPU path is not serving frames, for the agent to report: the retirement cause
    /// when this instance is retired, else the last slot-creation failure, else None.
    pub fn off_reason(&self) -> Option<String> {
        if let Some(retired) = self.retired() {
            return Some(format!("{} ({:?})", retired.reason.name(), retired.fault));
        }
        self.refusal.lock().ok().and_then(|refusal| refusal.clone())
    }
    /// Runs the slot-creation and handoff steps once on synthetic data and names each outcome.
    /// Diagnostic only: it creates one shared texture, two shared fences and one lease, and
    /// never touches a captured window.
    pub fn diagnose(self: &Arc<Self>, size: PixelSize) -> Vec<(&'static str, Result<(), String>)> {
        fn text(error: PlatformError) -> String {
            format!("{error:?}")
        }
        let mut steps = vec![("healthy", self.healthy().map_err(text))];
        steps.push((
            "shared_texture",
            self.shared(size, DXGI_FORMAT_B8G8R8A8_UNORM)
                .map(drop)
                .map_err(text),
        ));
        steps.push(("shared_fence", self.fence().map(drop).map_err(text)));
        let surface = match Surface::new(self.clone(), size) {
            Ok(surface) => {
                steps.push(("surface", Ok(())));
                surface
            }
            Err(error) => {
                steps.push(("surface", Err(text(error))));
                return steps;
            }
        };
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        let epoch = gate.epoch();
        match surface.begin(self, gate, epoch) {
            Ok(mut lease) => {
                steps.push(("begin", Ok(())));
                steps.push(("finish", lease.finish().map_err(text)));
            }
            Err(error) => steps.push(("begin", Err(text(error)))),
        }
        steps
    }
    /// Defers the release of `payload` until `fence` reaches `value` (checked by `healthy`, and
    /// waited for in `Drop`). The list is owned by this context, which is what ties the lifetime
    /// of the payload to it; the entries deliberately hold no `Arc<WindowsGpu>` of their own,
    /// because that made a cycle that kept a retired context alive for the whole process.
    pub(crate) fn hold<T: Send + 'static>(&self, fence: ID3D12Fence, value: u64, payload: T) {
        let held = Retained {
            _payload: Box::new(payload),
            fence,
            value,
        };
        if let Ok(mut retained) = self.retained.lock() {
            retained.push(held);
        } else {
            std::mem::forget(held);
        } // Poisoned ownership is unclean, never a safe early free.
    }
    pub(crate) fn generation(&self) -> Result<u64, PlatformError> {
        self.next
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .map_err(|_| error(Reason::Removed))
    }
    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, ()>, PlatformError> {
        let until = Instant::now() + BOUND;
        loop {
            self.healthy()?;
            match self.serial.try_lock() {
                Ok(lock) => return Ok(lock),
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    // A panic inside a lease leaves the queue state unknown.
                    self.retire();
                    return Err(error(Reason::Submitted));
                }
                Err(std::sync::TryLockError::WouldBlock) => (),
            }
            if Instant::now() >= until {
                return Err(PlatformError::Timeout);
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
    /// Waits for `fence`; a timeout retires the whole instance (the NV12 path relies on this).
    pub(crate) fn wait(
        &self,
        fence: &ID3D12Fence,
        value: u64,
        permitted: impl Fn() -> bool,
    ) -> Result<(), PlatformError> {
        self.wait_with(fence, value, permitted, true)
    }
    /// As `wait`, but with `retire_on_timeout` false a timeout is only reported: the caller
    /// quarantines its own surface and counts a strike, so one slow frame cannot cost the
    /// process its GPU path. A removed device retires the instance either way.
    pub(crate) fn wait_with(
        &self,
        fence: &ID3D12Fence,
        value: u64,
        permitted: impl Fn() -> bool,
        retire_on_timeout: bool,
    ) -> Result<(), PlatformError> {
        let until = Instant::now() + BOUND;
        loop {
            self.healthy()?;
            if !permitted() {
                return Err(PlatformError::Locked);
            }
            // SAFETY: live owned fence, polled without events or an unbounded native wait.
            let done = unsafe { fence.GetCompletedValue() };
            if done == u64::MAX {
                self.retire_for(model::Fault::Device, Reason::Removed);
                return Err(error(Reason::Removed));
            }
            if done >= value {
                return Ok(());
            }
            if Instant::now() >= until {
                if retire_on_timeout {
                    self.retire();
                }
                return Err(PlatformError::Timeout);
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
    pub(crate) fn shared(
        &self,
        size: PixelSize,
        format: DXGI_FORMAT,
    ) -> Result<SharedTexture, PlatformError> {
        self.healthy()?;
        model::texture_admit(size, self.device.limits().max_texture_dimension_2d).map_err(error)?;
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(size.width),
            Height: size.height,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            // ALLOW_RENDER_TARGET is what lets D3D11 open the texture: without it
            // OpenSharedResource1 answers E_INVALIDARG on the NVIDIA driver here (measured with
            // examples/gpu_share_variants.rs), which left every capture slot on the CPU path.
            // The D3D11 side only ever copies into it; the flag is not used for rendering.
            Flags: D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS
                | D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ..Default::default()
        };
        let mut resource = None;
        // SAFETY: a same-adapter, single-sample, DEFAULT shared committed resource in COMMON.
        unsafe {
            step(
                "CreateCommittedResource",
                self.d12.CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_DEFAULT,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_SHARED,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut resource,
                ),
            )?;
        }
        let resource = resource.ok_or_else(|| error(Reason::Sharing))?;
        // SAFETY: unnamed, non-inheritable own handle, opened only on the verified capture device.
        let handle = model::SharedHandle::new(
            unsafe {
                step(
                    "CreateSharedHandle(texture)",
                    self.d12
                        .CreateSharedHandle(&resource, None, GENERIC_ALL.0, None),
                )?
            },
            close_handle,
        );
        let device: ID3D11Device1 = api(self.d11.cast())?;
        // SAFETY: matching LUID and the exact handle owned above. Handle closes on every path.
        let texture: ID3D11Texture2D = unsafe {
            step(
                "OpenSharedResource1",
                device.OpenSharedResource1(handle.get()),
            )?
        };
        let mut actual = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: verifies the opened resource, without querying any window or other process.
        unsafe {
            texture.GetDesc(&mut actual);
        }
        if actual.Width != size.width
            || actual.Height != size.height
            || actual.Format != format
            || actual.MipLevels != 1
            || actual.ArraySize != 1
            || actual.SampleDesc.Count != 1
        {
            return Err(error(Reason::Sharing));
        }
        Ok(SharedTexture {
            resource,
            texture,
            desc,
        })
    }
    pub(crate) fn fence(&self) -> Result<Fence, PlatformError> {
        // SAFETY: shared fence on our device; own unnamed non-inheritable NT handle.
        let d12: ID3D12Fence = unsafe { api(self.d12.CreateFence(0, D3D12_FENCE_FLAG_SHARED))? };
        let handle = model::SharedHandle::new(
            // SAFETY: an owned unnamed noninheritable NT handle for this same-device shared fence.
            unsafe { api(self.d12.CreateSharedHandle(&d12, None, GENERIC_ALL.0, None))? },
            close_handle,
        );
        let device: ID3D11Device5 = api(self.d11.cast())?;
        let mut d11 = None;
        // SAFETY: opens this fence on the same-adapter device; initialized interface output.
        unsafe {
            api(device.OpenSharedFence(handle.get(), &mut d11))?;
        }
        Ok(Fence {
            d12,
            d11: d11.ok_or_else(|| error(Reason::Sharing))?,
        })
    }
}
fn close_handle(handle: HANDLE) {
    // SAFETY: exactly this CreateSharedHandle ownership, never borrowed or inherited.
    unsafe {
        let _ = CloseHandle(handle);
    }
}
pub(crate) struct SharedTexture {
    pub resource: ID3D12Resource,
    pub texture: ID3D11Texture2D,
    pub desc: D3D12_RESOURCE_DESC,
}
pub(crate) struct Fence {
    pub d12: ID3D12Fence,
    pub d11: ID3D11Fence,
}
/// Resources of a finished surface that the GPU may still be reading. `_payload` is declared
/// before `fence` so the resources are released first; the entry lives in `WindowsGpu::retained`.
struct Retained {
    _payload: Box<dyn Send>,
    fence: ID3D12Fence,
    value: u64,
}

/// One shared capture slot. Fields drop in declaration order: every native D3D12/D3D11 object
/// and the wgpu texture wrapping one are released before `gpu`, so a surface holding the last
/// reference to its context never tears the context down underneath its own resources.
pub(crate) struct Surface {
    pub shared: SharedTexture,
    producer: Fence,
    consumer: Fence,
    produced: AtomicU64,
    state: Mutex<Slot>,
    wrapped: Mutex<Option<wgpu::Texture>>,
    gpu: Arc<WindowsGpu>,
}
impl Surface {
    pub(crate) fn new(gpu: Arc<WindowsGpu>, size: PixelSize) -> Result<Arc<Self>, PlatformError> {
        let generation = gpu.generation()?;
        Ok(Arc::new(Self {
            shared: gpu.shared(size, DXGI_FORMAT_B8G8R8A8_UNORM)?,
            producer: gpu.fence()?,
            consumer: gpu.fence()?,
            produced: AtomicU64::new(0),
            state: Mutex::new(Slot::new(generation)),
            wrapped: Mutex::new(None),
            gpu,
        }))
    }
    pub(crate) fn free(&self, only_pool: bool) -> bool {
        // SAFETY: live retained own fence, no wait or resource mutation.
        let done = unsafe { self.consumer.d12.GetCompletedValue() };
        self.gpu.healthy().is_ok()
            && self
                .state
                .lock()
                .is_ok_and(|state| state.free(only_pool, done))
    }
    pub(crate) fn readable(&self) -> bool {
        self.free(true)
    }
    pub(crate) fn healthy(&self) -> bool {
        self.gpu.healthy().is_ok() && self.state.lock().is_ok_and(|state| !state.retired)
    }
    /// An unconfirmed handoff makes this surface unusable for good, whatever happens to the
    /// device. Its owner replaces it; its resources stay held until the consumer fence passes.
    pub(crate) fn quarantine(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.retire();
    }
    pub(crate) fn belongs(&self, gpu: &WindowsGpu) -> bool {
        std::ptr::eq(self.gpu.as_ref(), gpu)
    }
    pub(crate) fn copied(&self, context: &ID3D11DeviceContext) -> Result<(), PlatformError> {
        let value = self
            .produced
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .map_err(|_| error(Reason::Removed))?
            + 1;
        let context4: ID3D11DeviceContext4 = api(context.cast())?;
        // SAFETY: same context as the preceding copy, signal submitted by Flush; fence remains owned.
        unsafe {
            api(context4.Signal(&self.producer.d11, value))?;
            context.Flush();
        }
        Ok(())
    }
    pub(crate) fn begin<'a>(
        &'a self,
        gpu: &'a WindowsGpu,
        gate: Arc<IoGate>,
        epoch: u64,
    ) -> Result<Lease<'a>, PlatformError> {
        if !std::ptr::eq(self.gpu.as_ref(), gpu) {
            return Err(error(Reason::Adapter));
        }
        model::permitted(gate.is_open(), gate.epoch(), epoch).map_err(error)?;
        let serial = gpu.lock()?;
        let produced = self.produced.load(Ordering::Acquire);
        if let Err(cause) = gpu.wait_with(
            &self.producer.d12,
            produced,
            || gate.is_open() && gate.epoch() == epoch,
            false,
        ) {
            if matches!(cause, PlatformError::Timeout) {
                // The capture device never signalled: this surface is suspect, and enough of
                // these in a row mean the queue is stalled.
                self.quarantine();
                gpu.strike();
            }
            return Err(cause);
        }
        let mut wrapped = self.wrapped.lock().map_err(|_| error(Reason::Sharing))?;
        if wrapped.is_none() {
            let size = wgpu::Extent3d {
                width: self.shared.desc.Width as u32,
                height: self.shared.desc.Height,
                depth_or_array_layers: 1,
            };
            // SAFETY: owned resource, verified BGRA descriptor, first copy completed, actual COMMON state.
            let hal = unsafe {
                wgpu::hal::dx12::Device::texture_from_raw(
                    self.shared.resource.clone(),
                    wgpu::TextureFormat::Bgra8Unorm,
                    wgpu::TextureDimension::D2,
                    size,
                    1,
                    1,
                )
            };
            // SAFETY: exact initialized descriptor and state; HAL object owns its COM resource reference.
            *wrapped = Some(unsafe {
                gpu.device.create_texture_from_hal::<wgpu::hal::api::Dx12>(
                    hal,
                    &wgpu::TextureDescriptor {
                        label: Some("WGC shared slot"),
                        size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::Bgra8Unorm,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING,
                        view_formats: &[],
                    },
                    wgpu::TextureUses::PRESENT,
                )
            });
            let mut state = self.state.lock().map_err(|_| error(Reason::Sharing))?;
            let generation = state.generation;
            state.wrap(generation).map_err(error)?;
        }
        let texture = wrapped
            .as_ref()
            .ok_or_else(|| error(Reason::Sharing))?
            .clone();
        drop(wrapped);
        let mut state = self.state.lock().map_err(|_| error(Reason::Sharing))?;
        let done = state
            .done
            .checked_add(1)
            .ok_or_else(|| error(Reason::Removed))?;
        state.done = done;
        drop(state);
        let mut handoff = Handoff::default();
        handoff.stage().map_err(error)?;
        // SAFETY: scoped guard; staged wait is removed or consumed before the serial lock is released.
        let Some(queue) = (unsafe { gpu.queue.as_hal::<wgpu::hal::api::Dx12>() }) else {
            // `done` was already advanced for a signal that will never come.
            self.quarantine();
            return Err(error(Reason::Adapter));
        };
        queue.add_wait_fence(self.producer.d12.clone(), produced);
        drop(queue);
        Ok(Lease {
            surface: self,
            _serial: serial,
            texture,
            handoff,
            done,
            gate,
            epoch,
            failed: false,
        })
    }
}
impl Drop for Surface {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: read-only completion query on the fence whose resources we still own.
        let done = unsafe { self.consumer.d12.GetCompletedValue() };
        if done != u64::MAX && done < state.done {
            self.gpu.hold(
                self.consumer.d12.clone(),
                state.done,
                (
                    self.shared.resource.clone(),
                    self.shared.texture.clone(),
                    self.producer.d12.clone(),
                    self.producer.d11.clone(),
                    self.consumer.d11.clone(),
                    self.wrapped
                        .get_mut()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take(),
                ),
            );
        }
    }
}
/// Holds capture memory and source-queue ownership through scan, gather and native conversion.
pub struct Lease<'a> {
    surface: &'a Surface,
    _serial: MutexGuard<'a, ()>,
    pub texture: wgpu::Texture,
    handoff: Handoff,
    done: u64,
    gate: Arc<IoGate>,
    epoch: u64,
    failed: bool,
}
impl fmt::Debug for Lease<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowsCaptureLease")
            .field("done", &self.done)
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}
impl Lease<'_> {
    pub fn check(&self) -> Result<(), PlatformError> {
        model::permitted(self.gate.is_open(), self.gate.epoch(), self.epoch).map_err(error)
    }
    pub fn admission(&self) -> (Arc<IoGate>, u64) {
        (self.gate.clone(), self.epoch)
    }
    pub fn belongs(&self, gpu: &WindowsGpu) -> bool {
        self.surface.belongs(gpu)
    }
    pub fn submitted(&mut self) {
        self.handoff.submitted();
    }
    pub fn finish(&mut self) -> Result<(), PlatformError> {
        if self.handoff.complete {
            return Ok(());
        }
        if self.failed {
            // The first failure already quarantined the surface; a second attempt would only
            // submit and wait again on a handoff nobody can confirm.
            return Err(error(Reason::Submitted));
        }
        if let Err(cause) = self.release() {
            self.failed = true;
            self.surface.quarantine();
            if matches!(cause, PlatformError::Timeout) {
                self.surface.gpu.strike();
            }
            return Err(cause);
        }
        self.surface.gpu.settled();
        self.handoff.finish();
        model::permitted(self.gate.is_open(), self.gate.epoch(), self.epoch).map_err(error)
    }
    /// Returns the queue ownership taken in `begin`. Ok means the consumer fence passed.
    fn release(&mut self) -> Result<(), PlatformError> {
        let gpu = &self.surface.gpu;
        // SAFETY: staged entries belong to this serialized image handoff only.
        let queue = unsafe { gpu.queue.as_hal::<wgpu::hal::api::Dx12>() }
            .ok_or_else(|| error(Reason::Adapter))?;
        if !self.handoff.submitted {
            queue.remove_wait_fence(&self.surface.producer.d12);
            self.handoff.cancel();
        }
        queue.add_signal_fence(self.surface.consumer.d12.clone(), self.done);
        drop(queue);
        let mut commands = gpu.device.create_command_encoder(&Default::default());
        // Simultaneous-access resources decay to COMMON; Microsoft permits promotable
        // BeforeState values, so wgpu's explicit tracker transition remains valid after decay.
        commands.transition_resources(
            std::iter::empty(),
            std::iter::once(wgpu::TextureTransition {
                texture: &self.texture,
                selector: None,
                state: wgpu::TextureUses::PRESENT,
            }),
        );
        gpu.queue.submit([commands.finish()]);
        // Ownership cleanup proceeds even after gate closure; output publication does not. A
        // timeout here costs this surface (and a strike), not the whole device.
        gpu.wait_with(&self.surface.consumer.d12, self.done, || true, false)
    }
}
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        // `finish` quarantines the surface and counts the strike itself when it fails.
        let _ = self.finish();
    }
}
