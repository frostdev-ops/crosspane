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
        Arc, Mutex, MutexGuard,
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

pub(crate) const BOUND: Duration = Duration::from_secs(2);
pub(crate) fn error(reason: Reason) -> PlatformError {
    PlatformError::Backend(reason.name().into())
}
pub(crate) fn api<T>(value: windows::core::Result<T>) -> Result<T, PlatformError> {
    value.map_err(|e| PlatformError::Backend(format!("GPU HRESULT {:08x}", e.code().0 as u32)))
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
pub struct WindowsGpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub(crate) d11: ID3D11Device,
    pub(crate) context: Arc<Mutex<ID3D11DeviceContext>>,
    pub(crate) d12: ID3D12Device,
    pub(crate) raw_queue: ID3D12CommandQueue,
    pub(crate) luid: u64,
    pub(crate) video_supported: bool,
    serial: Mutex<()>,
    next: AtomicU64,
    failed: AtomicBool,
    retained: Mutex<Vec<Retained>>,
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
            device,
            queue,
            d11,
            context,
            d12,
            raw_queue,
            luid: luid(capture.AdapterLuid),
            video_supported,
            serial: Mutex::new(()),
            next: AtomicU64::new(1),
            failed: AtomicBool::new(false),
            retained: Mutex::new(Vec::new()),
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
        if self.failed.load(Ordering::Acquire) {
            return Err(error(Reason::Submitted));
        }
        // SAFETY: read-only removal status of our retained devices, not an owner session query.
        unsafe {
            api(self.d11.GetDeviceRemovedReason())?;
            api(self.d12.GetDeviceRemovedReason())?;
        }
        Ok(())
    }
    pub(crate) fn retire(&self) {
        self.failed.store(true, Ordering::Release);
    }
    pub(crate) fn hold<T: Send + 'static>(
        self: &Arc<Self>,
        fence: ID3D12Fence,
        value: u64,
        payload: T,
    ) {
        let held = Retained {
            _gpu: self.clone(),
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
                Err(std::sync::TryLockError::Poisoned(_)) => return Err(error(Reason::Submitted)),
                Err(std::sync::TryLockError::WouldBlock) => (),
            }
            if Instant::now() >= until {
                return Err(PlatformError::Timeout);
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
    pub(crate) fn wait(
        &self,
        fence: &ID3D12Fence,
        value: u64,
        permitted: impl Fn() -> bool,
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
                self.retire();
                return Err(error(Reason::Removed));
            }
            if done >= value {
                return Ok(());
            }
            if Instant::now() >= until {
                self.retire();
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
            Flags: D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
            ..Default::default()
        };
        let mut resource = None;
        // SAFETY: a same-adapter, single-sample, DEFAULT shared committed resource in COMMON.
        unsafe {
            api(self.d12.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_SHARED,
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut resource,
            ))?;
        }
        let resource = resource.ok_or_else(|| error(Reason::Sharing))?;
        // SAFETY: unnamed, non-inheritable own handle, opened only on the verified capture device.
        let handle = model::SharedHandle::new(
            unsafe {
                api(self
                    .d12
                    .CreateSharedHandle(&resource, None, GENERIC_ALL.0, None))?
            },
            close_handle,
        );
        let device: ID3D11Device1 = api(self.d11.cast())?;
        // SAFETY: matching LUID and the exact handle owned above. Handle closes on every path.
        let texture: ID3D11Texture2D = unsafe { api(device.OpenSharedResource1(handle.get()))? };
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
struct Retained {
    _gpu: Arc<WindowsGpu>,
    _payload: Box<dyn Send>,
    fence: ID3D12Fence,
    value: u64,
}

pub(crate) struct Surface {
    pub shared: SharedTexture,
    gpu: Arc<WindowsGpu>,
    producer: Fence,
    consumer: Fence,
    produced: AtomicU64,
    state: Mutex<Slot>,
    wrapped: Mutex<Option<wgpu::Texture>>,
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
        self.gpu.healthy().is_ok()
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
        gpu.wait(&self.producer.d12, produced, || {
            gate.is_open() && gate.epoch() == epoch
        })?;
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
        let queue = unsafe { gpu.queue.as_hal::<wgpu::hal::api::Dx12>() }
            .ok_or_else(|| error(Reason::Adapter))?;
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
        // Ownership cleanup proceeds even after gate closure; output publication does not.
        gpu.wait(&self.surface.consumer.d12, self.done, || true)?;
        self.handoff.finish();
        model::permitted(self.gate.is_open(), self.gate.epoch(), self.epoch).map_err(error)
    }
}
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if self.finish().is_err() && !self.handoff.complete {
            self.surface.gpu.retire();
            if let Ok(mut state) = self.surface.state.lock() {
                state.retire();
            }
        }
    }
}
