//! Native NV12 inputs and hardware MFT admission, using only public D3D/MF APIs.
use super::*;
use crate::{
    gpu::{Fence, SharedTexture, WindowsGpu},
    model::gpu::{CopyProgress, CopyStep, InputRelease, Layout, Plane, Reason},
};
use crosspane_media::codec::{NativeInput, NativeInputPool};
use std::{any::Any, sync::Mutex};
use windows::{
    Win32::Graphics::{Direct3D11::*, Direct3D12::*, Dxgi::Common::*},
    core::implement,
};

fn gpu_error(e: PlatformError) -> CodecError {
    failure(&e.to_string())
}
use crosspane_platform::PlatformError;

#[derive(Debug)]
pub struct Pool {
    pub(super) display: PixelSize,
    size: PixelSize,
    slots: Vec<Arc<InputSlot>>,
}
impl Pool {
    pub fn new(gpu: Arc<WindowsGpu>, display: PixelSize) -> Result<Arc<Self>, CodecError> {
        let size = Params::new(display, 1, 1)?.coded()?;
        Ok(Arc::new(Self {
            display,
            size,
            slots: (0..2)
                .map(|_| InputSlot::new(gpu.clone(), display))
                .collect::<Result<_, _>>()?,
        }))
    }
}
impl NativeInputPool for Pool {
    fn acquire(&self) -> Result<Arc<dyn NativeInput>, CodecError> {
        for slot in &self.slots {
            slot.gpu.healthy().map_err(gpu_error)?;
            // SAFETY: read-only query of this input's retained fence, no external resource access.
            let completed = unsafe { slot.fence.d12.GetCompletedValue() };
            let mut release = slot
                .release
                .lock()
                .map_err(|_| failure("input pool poisoned"))?;
            release.caller = Arc::strong_count(slot) == 1;
            release.gpu = completed != u64::MAX && completed >= slot.done.load(Ordering::Acquire);
            if release.free() {
                release
                    .claim(slot.gpu.generation().map_err(gpu_error)?)
                    .map_err(|_| failure("input acquisition isn't free"))?;
                *slot
                    .origin
                    .lock()
                    .map_err(|_| failure("input admission poisoned"))? = None;
                slot.written.store(false, Ordering::Release);
                return Ok(Arc::new(Input { slot: slot.clone() }));
            }
        }
        Err(failure(Reason::Busy.name()))
    }
    fn size(&self) -> PixelSize {
        self.size
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
struct Input {
    slot: Arc<InputSlot>,
}
impl std::fmt::Debug for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.slot.fmt(f)
    }
}
impl NativeInput for Input {
    fn size(&self) -> PixelSize {
        self.slot.layout.size
    }
    fn colour(&self) -> YuvColour {
        YuvColour::default()
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
pub struct InputSlot {
    gpu: Arc<WindowsGpu>,
    shared: SharedTexture,
    fence: Fence,
    buffer: wgpu::Buffer,
    layout: Layout,
    footprints: [D3D12_PLACED_SUBRESOURCE_FOOTPRINT; 2],
    commands: Mutex<(ID3D12CommandAllocator, ID3D12GraphicsCommandList)>,
    done: std::sync::atomic::AtomicU64,
    release: Mutex<InputRelease>,
    origin: Mutex<Option<(Arc<crosspane_platform::IoGate>, u64)>>,
    written: AtomicBool,
}
impl std::fmt::Debug for InputSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MfNv12Input")
            .field("size", &self.layout.size)
            .finish_non_exhaustive()
    }
}
impl InputSlot {
    fn new(gpu: Arc<WindowsGpu>, display: PixelSize) -> Result<Arc<Self>, CodecError> {
        let size = Params::new(display, 1, 1)?.coded()?;
        let shared = gpu.shared(size, DXGI_FORMAT_NV12).map_err(gpu_error)?;
        let mut footprints = [D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); 2];
        let mut rows = [0; 2];
        let mut row_bytes = [0; 2];
        let mut bytes = 0;
        // SAFETY: driver supplies footprints for the exact retained two-plane NV12 resource.
        unsafe {
            gpu.d12.GetCopyableFootprints(
                &shared.desc,
                0,
                2,
                0,
                Some(footprints.as_mut_ptr()),
                Some(rows.as_mut_ptr()),
                Some(row_bytes.as_mut_ptr()),
                Some(&mut bytes),
            );
        }
        if footprints[0].Footprint.Format != DXGI_FORMAT_R8_UNORM
            || footprints[1].Footprint.Format != DXGI_FORMAT_R8G8_UNORM
            || footprints.iter().any(|plane| plane.Footprint.Depth != 1)
            || rows != [size.height, size.height / 2]
            || row_bytes != [u64::from(size.width), u64::from(size.width)]
        {
            return Err(failure(Reason::Layout.name()));
        }
        let plane = |index: usize, bpp: u32| Plane {
            offset: footprints[index].Offset,
            pitch: footprints[index].Footprint.RowPitch,
            width: footprints[index].Footprint.Width,
            height: footprints[index].Footprint.Height,
            bytes_per_pixel: bpp,
        };
        // Include the final driver's row padding in the owned buffer allocation.
        bytes = bytes.max(
            footprints[1]
                .Offset
                .checked_add(
                    u64::from(footprints[1].Footprint.RowPitch)
                        .checked_mul(u64::from(rows[1]))
                        .ok_or_else(|| failure("NV12 row overflow"))?,
                )
                .ok_or_else(|| failure("NV12 storage overflow"))?,
        );
        let layout = Layout::validate(display, plane(0, 1), plane(1, 2), bytes)
            .map_err(|_| failure(Reason::Layout.name()))?;
        let limits = gpu.device.limits();
        if !layout.fits(
            limits.max_buffer_size,
            limits.max_storage_buffer_binding_size,
        ) {
            return Err(failure(Reason::Layout.name()));
        }
        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("MF GPU NV12 writer"),
            size: layout.bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        // SAFETY: native copy objects on the same source DX12 device; no commands submitted yet.
        let allocator: ID3D12CommandAllocator = unsafe {
            api(
                gpu.d12
                    .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT),
                "NV12 allocator",
            )?
        };
        // SAFETY: initially empty list uses this same-device allocator; no resource work has run.
        let list: ID3D12GraphicsCommandList = unsafe {
            api(
                gpu.d12
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None),
                "NV12 list",
            )?
        };
        // SAFETY: close initial empty list before its first Reset.
        unsafe {
            api(list.Close(), "initial NV12 close")?;
        }
        let generation = gpu.generation().map_err(gpu_error)?;
        let mut release = InputRelease::new(generation);
        release.native = true;
        release.gpu = true;
        Ok(Arc::new(Self {
            fence: gpu.fence().map_err(gpu_error)?,
            gpu,
            shared,
            buffer,
            layout,
            footprints,
            commands: Mutex::new((allocator, list)),
            done: std::sync::atomic::AtomicU64::new(0),
            release: Mutex::new(release),
            origin: Mutex::new(None),
            written: AtomicBool::new(false),
        }))
    }
    fn copy(&self) -> Result<(), CodecError> {
        self.gpu.healthy().map_err(gpu_error)?;
        if !self.permitted() {
            return Err(failure("native capture retired"));
        }
        let mut progress = CopyProgress::default();
        progress
            .advance(CopyStep::Shader)
            .map_err(|_| failure("NV12 write order"))?;
        let value = self
            .done
            .load(Ordering::Acquire)
            .checked_add(1)
            .ok_or_else(|| failure("NV12 fence exhausted"))?;
        let mut encoder = self.gpu.device.create_command_encoder(&Default::default());
        encoder.transition_resources(
            std::iter::once(wgpu::BufferTransition {
                buffer: &self.buffer,
                state: wgpu::BufferUses::COPY_SRC,
            }),
            std::iter::empty(),
        );
        self.gpu.queue.submit([encoder.finish()]);
        progress
            .advance(CopyStep::CopySource)
            .map_err(|_| failure("NV12 copy order"))?;
        // SAFETY: obtains a retained reference to this input buffer's owned DX12 resource.
        let buffer = unsafe { self.buffer.as_hal::<wgpu::hal::api::Dx12>() }
            .ok_or_else(|| failure("NV12 buffer backend mismatch"))?;
        // SAFETY: use only as a copy source after the tracked COPY_SRC submission.
        let raw = unsafe { buffer.raw_resource() }.clone();
        drop(buffer);
        let (allocator, list) = &*self
            .commands
            .lock()
            .map_err(|_| failure("NV12 command lock poisoned"))?;
        // SAFETY: previous native fence is complete before acquire; objects aren't concurrently used.
        unsafe {
            api(allocator.Reset(), "NV12 allocator Reset")?;
            api(list.Reset(allocator, None), "NV12 list Reset")?;
        }
        transition(
            list,
            &self.shared.resource,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        progress
            .advance(CopyStep::CopyDest)
            .map_err(|_| failure("NV12 copy order"))?;
        for (index, footprint) in self.footprints.iter().enumerate() {
            let mut source = D3D12_TEXTURE_COPY_LOCATION {
                pResource: ManuallyDrop::new(Some(raw.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: *footprint,
                },
            };
            let mut dest = D3D12_TEXTURE_COPY_LOCATION {
                pResource: ManuallyDrop::new(Some(self.shared.resource.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: index as u32,
                },
            };
            // SAFETY: validated footprint dimensions/pitches and matching plane resource bounds.
            unsafe {
                list.CopyTextureRegion(&dest, 0, 0, 0, &source, None);
                ManuallyDrop::drop(&mut source.pResource);
                ManuallyDrop::drop(&mut dest.pResource);
            }
        }
        transition(
            list,
            &self.shared.resource,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_COMMON,
        );
        progress
            .advance(CopyStep::Common)
            .map_err(|_| failure("NV12 copy order"))?;
        if !self.permitted() {
            return Err(failure("native capture retired"));
        }
        // SAFETY: same actual wgpu submission queue; tracker transition precedes native plane copies.
        unsafe {
            api(list.Close(), "NV12 list Close")?;
            let command = api(list.cast(), "NV12 command interface")?;
            // From this point completion is uncertain until the fence; even Signal failure
            // retains the submitted resources instead of mapping or reusing them.
            self.done.store(value, Ordering::Release);
            self.gpu.raw_queue.ExecuteCommandLists(&[Some(command)]);
            if let Err(e) = self.gpu.raw_queue.Signal(&self.fence.d12, value) {
                self.gpu.retire();
                return Err(failure(&e.to_string()));
            }
        }
        progress
            .advance(CopyStep::Signal)
            .map_err(|_| failure("NV12 copy order"))?;
        self.gpu
            .wait(&self.fence.d12, value, || self.permitted())
            .map_err(gpu_error)?;
        let until = Instant::now() + BOUND;
        let context = loop {
            self.gpu.healthy().map_err(gpu_error)?;
            if !self.permitted() {
                return Err(failure("native capture retired"));
            }
            match self.gpu.context.try_lock() {
                Ok(context) => break context,
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(failure("MF D3D context poisoned"));
                }
                Err(std::sync::TryLockError::WouldBlock) => (),
            }
            if Instant::now() >= until {
                self.gpu.retire();
                return Err(failure("MF D3D context timeout"));
            }
            thread::sleep(Duration::from_millis(1));
        };
        let context4: ID3D11DeviceContext4 = api(context.cast(), "MF context4")?;
        // SAFETY: completed GPU NV12 writer, same-device D3D11 view and shared fence.
        unsafe {
            api(context4.Wait(&self.fence.d11, value), "MF NV12 wait")?;
        }
        Ok(())
    }
    pub(super) fn permitted(&self) -> bool {
        self.origin.lock().is_ok_and(|origin| {
            origin
                .as_ref()
                .is_some_and(|(gate, epoch)| gate.is_open() && gate.epoch() == *epoch)
        })
    }
    pub(super) fn sample(
        self: &Arc<Self>,
        at: i64,
        duration: i64,
        bind: u32,
    ) -> Result<IMFSample, CodecError> {
        self.gpu.healthy().map_err(gpu_error)?;
        if !self.written.load(Ordering::Acquire) || !self.permitted() {
            return Err(failure("native capture retired or unwritten"));
        }
        let value = self.done.load(Ordering::Acquire);
        if value == 0 {
            return Err(failure("NV12 writer has not completed"));
        }
        self.gpu
            .wait(&self.fence.d12, value, || self.permitted())
            .map_err(gpu_error)?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: exact owned native input; verify all MFT-required bind flags rather than waiving them.
        unsafe {
            self.shared.texture.GetDesc(&mut desc);
        }
        if desc.BindFlags & bind != bind {
            return Err(failure("MFT NV12 bind flags unavailable"));
        }
        let callback: IMFAsyncCallback = Released {
            slot: self.clone(),
            generation: self
                .release
                .lock()
                .map_err(|_| failure("input ownership poisoned"))?
                .generation,
        }
        .into();
        // SAFETY: public tracked sample; DXGI buffer wraps our same-device NV12 texture without mapping.
        unsafe {
            let sample = api(MFCreateVideoSampleFromSurface(None), "MF tracked sample")?;
            let buffer = api(
                MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &self.shared.texture, 0, false),
                "MF DXGI surface",
            )?;
            api(sample.AddBuffer(&buffer), "MF native AddBuffer")?;
            api(sample.SetSampleTime(at), "native timestamp")?;
            api(sample.SetSampleDuration(duration), "native duration")?;
            let tracked: IMFTrackedSample = api(sample.cast(), "IMFTrackedSample")?;
            self.release
                .lock()
                .map_err(|_| failure("input ownership poisoned"))?
                .native = false;
            if let Err(error) = tracked.SetAllocator(&callback, None) {
                self.release
                    .lock()
                    .map_err(|_| failure("input ownership poisoned"))?
                    .native = true;
                return Err(failure(&error.to_string()));
            }
            Ok(sample)
        }
    }
}
impl Drop for InputSlot {
    fn drop(&mut self) {
        let value = self.done.load(Ordering::Acquire);
        // SAFETY: read-only completion query before releasing owned copy resources.
        let completed = unsafe { self.fence.d12.GetCompletedValue() };
        if completed != u64::MAX && completed < value {
            let (allocator, list) = self
                .commands
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.gpu.hold(
                self.fence.d12.clone(),
                value,
                (
                    self.shared.resource.clone(),
                    self.shared.texture.clone(),
                    self.buffer.clone(),
                    allocator.clone(),
                    list.clone(),
                    self.fence.d11.clone(),
                ),
            );
        }
    }
}
#[implement(IMFAsyncCallback)]
struct Released {
    slot: Arc<InputSlot>,
    generation: u64,
}
impl IMFAsyncCallback_Impl for Released_Impl {
    fn GetParameters(&self, _: *mut u32, _: *mut u32) -> windows::core::Result<()> {
        Err(windows::core::Error::from_hresult(
            windows::Win32::Foundation::E_NOTIMPL,
        ))
    }
    fn Invoke(&self, _: windows::core::Ref<'_, IMFAsyncResult>) -> windows::core::Result<()> {
        if let Ok(mut release) = self.slot.release.lock() {
            release.callback(self.generation);
        }
        Ok(())
    }
}
fn transition(
    list: &ID3D12GraphicsCommandList,
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) {
    let mut barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: ManuallyDrop::new(Some(resource.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    };
    // SAFETY: native-only texture's tracked states, exact matching owned barrier reference.
    unsafe {
        list.ResourceBarrier(std::slice::from_ref(&barrier));
        ManuallyDrop::drop(&mut (*barrier.Anonymous.Transition).pResource);
    }
}

pub fn nv12_buffer(input: &dyn NativeInput) -> Option<(&wgpu::Buffer, Layout)> {
    let input = input.as_any().downcast_ref::<Input>()?;
    Some((&input.slot.buffer, input.slot.layout))
}
pub fn finish_input(
    input: &dyn NativeInput,
    lease: &crate::gpu::Lease<'_>,
) -> Result<(), CodecError> {
    let input = input
        .as_any()
        .downcast_ref::<Input>()
        .ok_or(CodecError::BadInput("foreign MF native input"))?;
    if !lease.belongs(&input.slot.gpu) {
        return Err(CodecError::BadInput("MF source device mismatch"));
    }
    lease.check().map_err(gpu_error)?;
    *input
        .slot
        .origin
        .lock()
        .map_err(|_| failure("input admission poisoned"))? = Some(lease.admission());
    input.slot.copy()?;
    lease.check().map_err(gpu_error)?;
    input.slot.written.store(true, Ordering::Release);
    Ok(())
}
pub(super) fn lease(input: &dyn NativeInput, pool: &Pool) -> Result<Arc<InputSlot>, CodecError> {
    let slot = input
        .as_any()
        .downcast_ref::<Input>()
        .map(|input| input.slot.clone())
        .ok_or(CodecError::BadInput("foreign MF native input"))?;
    if !pool.slots.iter().any(|own| Arc::ptr_eq(own, &slot)) {
        return Err(CodecError::BadInput("MF input belongs to another encoder"));
    }
    Ok(slot)
}
pub(super) fn select(
    gpu: &Arc<WindowsGpu>,
    params: Params,
    deadline: Instant,
    stop: &AtomicBool,
) -> Result<Session, CodecError> {
    gpu.healthy().map_err(gpu_error)?;
    if !gpu.video_supported {
        return Err(failure(Reason::Mft.name()));
    }
    let mut token = 0;
    let mut manager = None;
    // SAFETY: initialized MTA/MF worker, manager retains our same-adapter protected device.
    unsafe {
        api(
            MFCreateDXGIDeviceManager(&mut token, &mut manager),
            "MF DXGI manager",
        )?;
    }
    let manager = manager.ok_or_else(|| failure("missing DXGI manager"))?;
    // SAFETY: the manager's own creation token and our retained device.
    unsafe {
        api(manager.ResetDevice(&gpu.d11, token), "MF ResetDevice")?;
    }
    let mut items = ptr::null_mut();
    let mut count = 0;
    let mut attrs = None;
    // SAFETY: bounded typed enumeration, LUID-filtered hardware H264 encoders only.
    unsafe {
        api(MFCreateAttributes(&mut attrs, 1), "MFT adapter attrs")?;
        let attrs = attrs.ok_or_else(|| failure("missing adapter attrs"))?;
        api(
            attrs.SetUINT64(&MFT_ENUM_ADAPTER_LUID, gpu.luid),
            "MFT adapter LUID",
        )?;
        api(
            MFTEnum2(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&MFT_REGISTER_TYPE_INFO {
                    guidMajorType: MFMediaType_Video,
                    guidSubtype: MFVideoFormat_NV12,
                }),
                Some(&MFT_REGISTER_TYPE_INFO {
                    guidMajorType: MFMediaType_Video,
                    guidSubtype: MFVideoFormat_H264,
                }),
                &attrs,
                &mut items,
                &mut count,
            ),
            "MFTEnum2",
        )?;
    }
    if items.is_null() {
        return Err(failure("no matching GPU MFT"));
    }
    let mut candidates = Vec::new();
    // SAFETY: exactly count owned activation entries; always release the CoTaskMem array.
    unsafe {
        for item in std::slice::from_raw_parts_mut(items, count as usize) {
            if let Some(item) = item.take() {
                candidates.push(item);
            }
        }
        CoTaskMemFree(Some(items.cast()));
    }
    for candidate in candidates {
        check(deadline, stop)?;
        // SAFETY: activation and metadata from our exact adapter-filtered candidate.
        let result = unsafe { candidate.ActivateObject::<IMFTransform>() }
            .map_err(|_| failure("GPU activation failed"))
            .and_then(|transform| {
                // SAFETY: query the public awareness attribute before handing it a device manager.
                let aware = unsafe {
                    transform
                        .GetAttributes()
                        .and_then(|attrs| attrs.GetUINT32(&MF_SA_D3D11_AWARE))
                }
                .ok()
                    == Some(1);
                crate::model::gpu::mft_admit(true, aware, true)
                    .map_err(|_| failure("MFT isn't D3D11-aware"))?;
                Session::configure_with_manager(
                    transform,
                    true,
                    format!("{} / GPU input", friendly_name(&candidate)),
                    Mode::Encoder(params),
                    Some(&manager),
                )
            });
        if result.is_ok() {
            return result;
        }
        // SAFETY: failed candidate has no accepted session; release activation cache.
        unsafe {
            let _ = candidate.ShutdownObject();
        }
    }
    Err(failure("GPU MFT admission refused"))
}
