//! OPAQUE_FD Vulkan allocations imported once into FFmpeg's CUDA context.
use std::{
    any::Any,
    ffi::{CString, c_void},
    os::fd::{FromRawFd, IntoRawFd, OwnedFd},
    ptr,
    sync::{Arc, Mutex, Weak},
};

use ash::vk;
use crosspane_media::codec::{NativeInput, NativeInputPool};
use libloading::Library;
use wgpu::hal::api::Vulkan;

use super::*;

/// Where the NV12 writer must put a pool buffer's planes (bytes, multiples of 256).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nv12Layout {
    pub y_offset: u64,
    pub y_pitch: u32,
    pub uv_offset: u64,
    pub uv_pitch: u32,
}

/// The STORAGE | COPY_DST buffer and layout of one of this crate's native inputs.
pub fn nv12_buffer(input: &dyn NativeInput) -> Option<(&wgpu::Buffer, Nv12Layout)> {
    let input = input.as_any().downcast_ref::<Input>()?;
    Some((&input.lease.slot.buffer, input.lease.slot.layout))
}

// CUDA driver ABI, from NVIDIA's MIT-licensed ffnvcodec/dynlink_cuda.h.
// The union is 16 bytes, pointer-aligned, including its unused Win32 variant.
#[repr(C)]
union MemoryHandle {
    fd: i32,
    win32: [*mut c_void; 2],
}
#[repr(C)]
struct MemoryDesc {
    kind: i32,
    handle: MemoryHandle,
    size: u64,
    flags: u32,
    reserved: [u32; 16],
}
#[repr(C)]
struct BufferDesc {
    offset: u64,
    size: u64,
    flags: u32,
    reserved: [u32; 16],
}
type Context = *mut c_void;
type ExternalMemory = *mut c_void;

struct Driver {
    _library: Library,
    init: unsafe extern "C" fn(u32) -> i32,
    count: unsafe extern "C" fn(*mut i32) -> i32,
    device: unsafe extern "C" fn(*mut i32, i32) -> i32,
    uuid: unsafe extern "C" fn(*mut [u8; 16], i32) -> i32,
    push: unsafe extern "C" fn(Context) -> i32,
    pop: unsafe extern "C" fn(*mut Context) -> i32,
    import: unsafe extern "C" fn(*mut ExternalMemory, *const MemoryDesc) -> i32,
    map: unsafe extern "C" fn(*mut u64, ExternalMemory, *const BufferDesc) -> i32,
    destroy: unsafe extern "C" fn(ExternalMemory) -> i32,
    free: unsafe extern "C" fn(u64) -> i32,
}
impl Driver {
    fn load() -> Result<Self, CodecError> {
        // SAFETY: load the system CUDA driver; every symbol below has the driver ABI above.
        unsafe {
            let library = Library::new("libcuda.so.1").map_err(failed)?;
            let driver = Self {
                init: *library.get(b"cuInit\0").map_err(failed)?,
                count: *library.get(b"cuDeviceGetCount\0").map_err(failed)?,
                device: *library.get(b"cuDeviceGet\0").map_err(failed)?,
                uuid: *library.get(b"cuDeviceGetUuid_v2\0").map_err(failed)?,
                push: *library.get(b"cuCtxPushCurrent_v2\0").map_err(failed)?,
                pop: *library.get(b"cuCtxPopCurrent_v2\0").map_err(failed)?,
                import: *library.get(b"cuImportExternalMemory\0").map_err(failed)?,
                map: *library
                    .get(b"cuExternalMemoryGetMappedBuffer\0")
                    .map_err(failed)?,
                destroy: *library.get(b"cuDestroyExternalMemory\0").map_err(failed)?,
                free: *library.get(b"cuMemFree_v2\0").map_err(failed)?,
                _library: library,
            };
            check((driver.init)(0), "cuInit")?;
            Ok(driver)
        }
    }
}
fn failed(error: impl std::fmt::Display) -> CodecError {
    CodecError::Failed(error.to_string())
}

fn check(result: i32, operation: &str) -> Result<(), CodecError> {
    if result == 0 {
        Ok(())
    } else {
        Err(CodecError::Failed(format!(
            "{operation}: CUresult {result}"
        )))
    }
}

pub(super) struct HwRef(pub *mut ffmpeg::ffi::AVBufferRef);
// SAFETY: AVBufferRefs are reference counted; the pointer is owned, never mutated except at drop.
unsafe impl Send for HwRef {}
// SAFETY: shared references only read the context; FFmpeg and CUDA support concurrent contexts.
unsafe impl Sync for HwRef {}
impl Drop for HwRef {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns exactly one reference, including on setup failure.
        unsafe { ffmpeg::ffi::av_buffer_unref(&mut self.0) };
    }
}
impl HwRef {
    pub(super) fn reference(&self) -> Result<*mut ffmpeg::ffi::AVBufferRef, CodecError> {
        // SAFETY: the owned buffer remains alive throughout the reference operation.
        let reference = unsafe { ffmpeg::ffi::av_buffer_ref(self.0) };
        if reference.is_null() {
            Err(CodecError::Failed("av_buffer_ref failed".into()))
        } else {
            Ok(reference)
        }
    }
}

struct Cuda {
    driver: Driver,
    context: Context,
    device: HwRef,
}
// SAFETY: context is kept alive by device; CUDA current-context stacks are thread-local.
unsafe impl Send for Cuda {}
// SAFETY: operations push/pop the context on the calling thread; CUDA contexts are thread safe.
unsafe impl Sync for Cuda {}
struct Current<'a>(&'a Cuda);
impl Cuda {
    fn enter(&self) -> Result<Current<'_>, CodecError> {
        // SAFETY: device owns this live context, on this thread's context stack.
        let result = unsafe { (self.driver.push)(self.context) };
        check(result, "cuCtxPushCurrent")?;
        Ok(Current(self))
    }
}
impl Drop for Current<'_> {
    fn drop(&mut self) {
        let mut previous = ptr::null_mut();
        // SAFETY: balances the successful push on the same thread.
        let result = unsafe { (self.0.driver.pop)(&mut previous) };
        if result != 0 {
            tracing::error!(result, "cuCtxPopCurrent failed");
        }
    }
}

pub(super) struct Source {
    device: wgpu::Device,
    cuda: Arc<Cuda>,
}
impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CudaSource").finish_non_exhaustive()
    }
}
impl Source {
    pub(super) fn new(device: wgpu::Device) -> Result<Self, CodecError> {
        let driver = Driver::load()?;
        // SAFETY: only query the HAL and physical properties, without mutating wgpu state.
        let uuid = unsafe {
            let hal = device
                .as_hal::<Vulkan>()
                .ok_or_else(|| CodecError::Failed("source is not Vulkan".into()))?;
            if !hal
                .enabled_device_extensions()
                .contains(&ash::khr::external_memory_fd::NAME)
            {
                return Err(CodecError::Failed(
                    "wgpu device lacks VK_KHR_external_memory_fd".into(),
                ));
            }
            let mut id = vk::PhysicalDeviceIDProperties::default();
            let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
            hal.shared_instance()
                .raw_instance()
                .get_physical_device_properties2(hal.raw_physical_device(), &mut properties);
            id.device_uuid
        };
        let mut count = 0;
        // SAFETY: all output pointers have the declared ABI and valid storage.
        let ordinal = unsafe {
            check((driver.count)(&mut count), "cuDeviceGetCount")?;
            let mut found = None;
            for ordinal in 0..count {
                let mut cuda_device = 0;
                let mut cuda_uuid = [0; 16];
                check((driver.device)(&mut cuda_device, ordinal), "cuDeviceGet")?;
                check(
                    (driver.uuid)(&mut cuda_uuid, cuda_device),
                    "cuDeviceGetUuid_v2",
                )?;
                if cuda_uuid == uuid {
                    found = Some(ordinal);
                    break;
                }
            }
            found.ok_or_else(|| CodecError::Failed("no CUDA device matches Vulkan UUID".into()))?
        };
        let ordinal_string = CString::new(ordinal.to_string()).map_err(failed)?;
        let mut reference = HwRef(ptr::null_mut());
        // SAFETY: FFmpeg creates and owns the device context; string lives through the call.
        let result = unsafe {
            ffmpeg::ffi::av_hwdevice_ctx_create(
                &mut reference.0,
                ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
                ordinal_string.as_ptr(),
                ptr::null_mut(),
                0,
            )
        };
        if result < 0 {
            return Err(failed(ffmpeg::Error::from(result)));
        }
        // SAFETY: successful CUDA creation gives AVHWDeviceContext with AVCUDADeviceContext hwctx.
        let context = unsafe {
            let hw = (*reference.0).data.cast::<ffmpeg::ffi::AVHWDeviceContext>();
            let cuda = (*hw).hwctx.cast::<ffmpeg::ffi::AVCUDADeviceContext>();
            (*cuda).cuda_ctx.cast::<c_void>()
        };
        tracing::info!(ordinal, ?uuid, "Vulkan source matches NVENC CUDA device");
        Ok(Self {
            device,
            cuda: Arc::new(Cuda {
                driver,
                context,
                device: reference,
            }),
        })
    }
    pub(super) fn pool(&self, size: PixelSize) -> Result<Arc<Pool>, CodecError> {
        let pitch = size
            .width
            .checked_add(255)
            .map(|width| width & !255)
            .filter(|pitch| *pitch <= i32::MAX as u32)
            .ok_or(CodecError::BadInput("NV12 pitch overflow"))?;
        let layout = Nv12Layout {
            y_offset: 0,
            y_pitch: pitch,
            uv_offset: u64::from(pitch) * u64::from(size.height),
            uv_pitch: pitch,
        };
        let length = layout.uv_offset + u64::from(pitch) * u64::from(size.height / 2);
        if length > self.device.limits().max_buffer_size {
            return Err(CodecError::Failed(
                "NV12 buffer exceeds device limit".into(),
            ));
        }
        let frames = frames(&self.cuda.device, size)?;
        let mut slots = Vec::with_capacity(4);
        for _ in 0..4 {
            slots.push(Arc::new(self.slot(size, layout, length)?));
        }
        Ok(Arc::new(Pool {
            size,
            slots,
            leases: Mutex::new((0..4).map(|_| Weak::new()).collect()),
            frames,
        }))
    }
    fn slot(&self, size: PixelSize, layout: Nv12Layout, length: u64) -> Result<Slot, CodecError> {
        // SAFETY: HAL guard keeps the live device; objects allocated here are independently owned.
        let (mut allocation, fd) = unsafe {
            let hal = self
                .device
                .as_hal::<Vulkan>()
                .ok_or_else(|| CodecError::Failed("Vulkan device lost".into()))?;
            let raw = hal.raw_device().clone();
            let instance = hal.shared_instance().raw_instance();
            let mut external = vk::ExternalMemoryBufferCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
            let info = vk::BufferCreateInfo::default()
                .size(length)
                .usage(vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .push_next(&mut external);
            let buffer = raw.create_buffer(&info, None).map_err(failed)?;
            let mut allocation = Allocation {
                raw,
                buffer,
                memory: vk::DeviceMemory::null(),
                mapping: None,
            };
            let requirements = allocation.raw.get_buffer_memory_requirements(buffer);
            let properties =
                instance.get_physical_device_memory_properties(hal.raw_physical_device());
            let memory_type = (0..properties.memory_type_count)
                .find(|index| {
                    requirements.memory_type_bits & (1 << index) != 0
                        && properties.memory_types[*index as usize]
                            .property_flags
                            .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
                })
                .ok_or_else(|| CodecError::Failed("no device-local memory".into()))?;
            let mut export = vk::ExportMemoryAllocateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(buffer);
            let info = vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(memory_type)
                .push_next(&mut export)
                .push_next(&mut dedicated);
            allocation.memory = allocation
                .raw
                .allocate_memory(&info, None)
                .map_err(failed)?;
            allocation
                .raw
                .bind_buffer_memory(buffer, allocation.memory, 0)
                .map_err(failed)?;
            let export_api = ash::khr::external_memory_fd::Device::new(instance, &allocation.raw);
            let info = vk::MemoryGetFdInfoKHR::default()
                .memory(allocation.memory)
                .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
            let fd = OwnedFd::from_raw_fd(export_api.get_memory_fd(&info).map_err(failed)?);
            (allocation, (fd, requirements.size))
        };
        let current = self.cuda.enter()?;
        let mut memory = ptr::null_mut();
        // Zero the whole union, including bytes not occupied by the fd.
        let mut handle = MemoryHandle {
            win32: [ptr::null_mut(); 2],
        };
        use std::os::fd::AsRawFd;
        handle.fd = fd.0.as_raw_fd();
        let desc = MemoryDesc {
            kind: 1,
            handle,
            size: fd.1,
            flags: 1,
            reserved: [0; 16],
        };
        // SAFETY: descriptor matches the dedicated OPAQUE_FD allocation. Success consumes the fd.
        let result = unsafe { (self.cuda.driver.import)(&mut memory, &desc) };
        check(result, "cuImportExternalMemory")?;
        let _consumed_fd = fd.0.into_raw_fd();
        let mut mapping = Mapping {
            cuda: Arc::clone(&self.cuda),
            memory,
            pointer: 0,
        };
        let desc = BufferDesc {
            offset: 0,
            size: length,
            flags: 0,
            reserved: [0; 16],
        };
        // SAFETY: mapping is inside the imported allocation; current is the importing context.
        let result = unsafe { (self.cuda.driver.map)(&mut mapping.pointer, memory, &desc) };
        let pointer = mapping.pointer;
        allocation.mapping = Some(mapping);
        check(result, "cuExternalMemoryGetMappedBuffer")?;
        drop(current);
        // SAFETY: callback owns buffer, CUDA mapping and Vulkan memory; it releases CUDA first.
        let buffer = unsafe {
            let hal_buffer = wgpu::hal::vulkan::Buffer::from_raw_externally_owned(
                allocation.buffer,
                Box::new(move || drop(allocation)),
            );
            self.device.create_buffer_from_hal::<Vulkan>(
                hal_buffer,
                &wgpu::BufferDescriptor {
                    label: Some("NVENC NV12 OPAQUE_FD"),
                    size: length,
                    usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                },
            )
        };
        Ok(Slot {
            buffer,
            pointer,
            layout,
            size,
        })
    }
}
struct Mapping {
    cuda: Arc<Cuda>,
    memory: ExternalMemory,
    pointer: u64,
}
// SAFETY: imported memory is context-owned and all operations enter that context on their thread.
unsafe impl Send for Mapping {}
// SAFETY: shared access never changes mapping; teardown requires exclusive ownership.
unsafe impl Sync for Mapping {}
impl Mapping {
    fn release(&mut self) -> Result<(), CodecError> {
        if self.memory.is_null() {
            return Ok(());
        }
        let _current = self.cuda.enter()?;
        // SAFETY: uniquely owned mapping, no GPU users remain when the HAL callback runs.
        unsafe {
            if self.pointer != 0 {
                check((self.cuda.driver.free)(self.pointer), "cuMemFree")?;
                self.pointer = 0;
            }
            check(
                (self.cuda.driver.destroy)(self.memory),
                "cuDestroyExternalMemory",
            )?;
            self.memory = ptr::null_mut();
        }
        Ok(())
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            tracing::error!(%error, "CUDA mapping teardown failed");
        }
    }
}

struct Allocation {
    raw: ash::Device,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapping: Option<Mapping>,
}
impl Drop for Allocation {
    fn drop(&mut self) {
        if let Some(mut mapping) = self.mapping.take()
            && let Err(error) = mapping.release()
        {
            // Preserve both allocations rather than free Vulkan memory still mapped by CUDA.
            tracing::error!(%error, "CUDA teardown failed; retaining external allocation");
            std::mem::forget(mapping);
            return;
        }
        // SAFETY: independent objects, no wgpu submissions or CUDA users remain at callback time.
        unsafe {
            self.raw.destroy_buffer(self.buffer, None);
            if self.memory != vk::DeviceMemory::null() {
                self.raw.free_memory(self.memory, None);
            }
        }
    }
}
fn frames(device: &HwRef, size: PixelSize) -> Result<HwRef, CodecError> {
    // SAFETY: device is a live CUDA AVHWDeviceContext; result is owned by the wrapper.
    let frames = HwRef(unsafe { ffmpeg::ffi::av_hwframe_ctx_alloc(device.0) });
    if frames.0.is_null() {
        return Err(CodecError::Failed("av_hwframe_ctx_alloc failed".into()));
    }
    // SAFETY: exclusive access during initialization, zero initial_pool_size allocates no frames.
    let result = unsafe {
        let context = (*frames.0).data.cast::<ffmpeg::ffi::AVHWFramesContext>();
        (*context).format = ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_CUDA;
        (*context).sw_format = ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NV12;
        (*context).width = size.width as i32;
        (*context).height = size.height as i32;
        (*context).initial_pool_size = 0;
        ffmpeg::ffi::av_hwframe_ctx_init(frames.0)
    };
    if result < 0 {
        return Err(failed(ffmpeg::Error::from(result)));
    }
    Ok(frames)
}
#[derive(Debug)]
struct Slot {
    buffer: wgpu::Buffer,
    pointer: u64,
    layout: Nv12Layout,
    size: PixelSize,
}
#[derive(Debug)]
struct Lease {
    slot: Arc<Slot>,
}
#[derive(Debug)]
struct Input {
    lease: Arc<Lease>,
}
impl NativeInput for Input {
    fn size(&self) -> PixelSize {
        self.lease.slot.size
    }
    fn colour(&self) -> YuvColour {
        YuvColour::default()
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
pub(super) struct Pool {
    size: PixelSize,
    slots: Vec<Arc<Slot>>,
    leases: Mutex<Vec<Weak<Lease>>>,
    pub(super) frames: HwRef,
}
impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CudaNv12Pool")
            .field("size", &self.size)
            .field("slots", &self.slots)
            .finish_non_exhaustive()
    }
}
impl NativeInputPool for Pool {
    fn acquire(&self) -> Result<Arc<dyn NativeInput>, CodecError> {
        let mut leases = self
            .leases
            .lock()
            .map_err(|_| CodecError::Failed("pool lock poisoned".into()))?;
        for (index, lease) in leases.iter_mut().enumerate() {
            if lease.strong_count() == 0 {
                let new = Arc::new(Lease {
                    slot: Arc::clone(&self.slots[index]),
                });
                *lease = Arc::downgrade(&new);
                return Ok(Arc::new(Input { lease: new }));
            }
        }
        Err(CodecError::Failed(
            "all four NV12 buffers are in flight".into(),
        ))
    }
    fn size(&self) -> PixelSize {
        self.size
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
impl Pool {
    pub(super) fn frame(&self, input: &dyn NativeInput) -> Result<frame::Video, CodecError> {
        let input = input
            .as_any()
            .downcast_ref::<Input>()
            .ok_or(CodecError::BadInput("not a CUDA NV12 input"))?;
        if !self
            .slots
            .iter()
            .any(|slot| Arc::ptr_eq(slot, &input.lease.slot))
        {
            return Err(CodecError::BadInput("input is from another pool"));
        }
        let slot = &input.lease.slot;
        let mut frame = frame::Video::empty();
        let reference = self.frames.reference()?;
        let lease = Box::into_raw(Box::new(Arc::clone(&input.lease)));
        // SAFETY: allocate AVBuffer metadata only; CUDA pointers are never CPU-dereferenced.
        // The release callback drops the lease, never frees data. FFmpeg retains it until NVENC
        // finishes reading. All planes fit in the slot and frames context matches its coded size.
        unsafe {
            let raw = frame.as_mut_ptr();
            (*raw).format = ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_CUDA as i32;
            (*raw).width = self.size.width as i32;
            (*raw).height = self.size.height as i32;
            (*raw).data[0] = (slot.pointer + slot.layout.y_offset) as *mut u8;
            (*raw).data[1] = (slot.pointer + slot.layout.uv_offset) as *mut u8;
            (*raw).linesize[0] = slot.layout.y_pitch as i32;
            (*raw).linesize[1] = slot.layout.uv_pitch as i32;
            (*raw).hw_frames_ctx = reference;
            (*raw).buf[0] = ffmpeg::ffi::av_buffer_create(
                (*raw).data[0],
                slot.buffer.size() as usize,
                Some(release_lease),
                lease.cast(),
                ffmpeg::ffi::AV_BUFFER_FLAG_READONLY,
            );
            if (*raw).buf[0].is_null() {
                drop(Box::from_raw(lease));
                return Err(CodecError::Failed("av_buffer_create failed".into()));
            }
        }
        Ok(frame)
    }
}
unsafe extern "C" fn release_lease(opaque: *mut c_void, _data: *mut u8) {
    // SAFETY: opaque is exactly the Box transferred to av_buffer_create, released once.
    unsafe { drop(Box::from_raw(opaque.cast::<Arc<Lease>>())) };
}
