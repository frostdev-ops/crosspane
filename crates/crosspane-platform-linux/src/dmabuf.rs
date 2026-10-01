//! Vulkan DMA-BUF capture allocations on the caller's shared wgpu device.
#![cfg(feature = "gpu")]

use std::any::Any;
use std::future::Future;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use ash::vk;
use crosspane_platform::{NativeImage, PlatformError};
use crosspane_types::geom::PixelSize;

fn error(e: impl std::fmt::Display) -> PlatformError {
    PlatformError::Backend(format!("DMA-BUF: {e}"))
}

struct ThreadWake(std::thread::Thread);
impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}
fn wait<T>(future: impl Future<Output = T>) -> T {
    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park_timeout(Duration::from_millis(10)),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Gpu {
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    pub(crate) node: u64,
    nvidia: bool,
}

impl Gpu {
    pub(crate) fn open(node: u64, wanted: wgpu::Features) -> Result<Self, PlatformError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        for adapter in wait(instance.enumerate_adapters(wgpu::Backends::VULKAN)) {
            // SAFETY: inspecting the adapter through its lifetime guard; no object is mutated.
            let Some(hal) = (unsafe { adapter.as_hal::<wgpu::hal::api::Vulkan>() }) else {
                continue;
            };
            let raw = hal.shared_instance().raw_instance();
            let physical = hal.raw_physical_device();
            // SAFETY: the physical device belongs to the guarded instance.
            let extensions =
                unsafe { raw.enumerate_device_extension_properties(physical) }.map_err(error)?;
            if !extensions.iter().any(|e| {
                e.extension_name_as_c_str().ok() == Some(ash::ext::physical_device_drm::NAME)
            }) {
                continue;
            }
            let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
            let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
            // SAFETY: the extension is supported and the output chain is valid for this query.
            unsafe { raw.get_physical_device_properties2(physical, &mut properties) };
            let matches = (drm.has_render != 0
                && rustix::fs::makedev(drm.render_major as u32, drm.render_minor as u32) == node)
                || (drm.has_primary != 0
                    && rustix::fs::makedev(drm.primary_major as u32, drm.primary_minor as u32)
                        == node);
            if !matches {
                continue;
            }
            drop(hal);
            let required = wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF;
            if !adapter.features().contains(required) {
                return Err(PlatformError::Unsupported(
                    "Vulkan DMA-BUF extensions required",
                ));
            }
            let (device, queue) = wait(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("crosspane capture"),
                required_features: (wanted & adapter.features()) | required,
                ..Default::default()
            }))
            .map_err(error)?;
            // SAFETY: only inspecting enabled extensions with the device guard held.
            let hal = unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }
                .ok_or_else(|| error("Vulkan device unavailable"))?;
            for extension in [
                ash::khr::external_memory_fd::NAME,
                ash::ext::external_memory_dma_buf::NAME,
                ash::ext::image_drm_format_modifier::NAME,
            ] {
                if !hal.enabled_device_extensions().contains(&extension) {
                    return Err(error(format!("required extension is off: {extension:?}")));
                }
            }
            drop(hal);
            return Ok(Self {
                device,
                queue,
                node,
                nvidia: adapter.get_info().vendor == 0x10de,
            });
        }
        Err(PlatformError::Unsupported(
            "no Vulkan adapter for compositor DRM device",
        ))
    }

    pub(crate) fn modifiers(&self, offered: &[u64]) -> Result<Vec<u64>, PlatformError> {
        // Integration-test fault injection is confined to an explicitly opted-in nested session.
        if std::env::var("CROSSPANE_NESTED_HYPR").as_deref() == Ok("1")
            && std::env::var("CROSSPANE_TEST_EMPTY_DMABUF_MODIFIERS").as_deref() == Ok("1")
        {
            return Ok(Vec::new());
        }
        // SAFETY: the guard protects all queried Vulkan handles.
        let hal = unsafe { self.device.as_hal::<wgpu::hal::api::Vulkan>() }
            .ok_or_else(|| error("Vulkan device unavailable"))?;
        let instance = hal.shared_instance().raw_instance();
        let physical = hal.raw_physical_device();
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut properties = vk::FormatProperties2::default().push_next(&mut list);
        // SAFETY: valid output chain; first call queries the list length.
        unsafe {
            instance.get_physical_device_format_properties2(
                physical,
                vk::Format::B8G8R8A8_UNORM,
                &mut properties,
            )
        };
        let mut entries = vec![
            vk::DrmFormatModifierPropertiesEXT::default();
            list.drm_format_modifier_count as usize
        ];
        list = list.drm_format_modifier_properties(&mut entries);
        let mut properties = vk::FormatProperties2::default().push_next(&mut list);
        // SAFETY: storage for the complete modifier list is live throughout this call.
        unsafe {
            instance.get_physical_device_format_properties2(
                physical,
                vk::Format::B8G8R8A8_UNORM,
                &mut properties,
            )
        };
        let mut result = Vec::new();
        for &modifier in offered {
            if self.nvidia && modifier == 0 {
                continue;
            }
            if !entries.iter().any(|entry| {
                entry.drm_format_modifier == modifier
                    && entry.drm_format_modifier_plane_count == 1
                    && entry.drm_format_modifier_tiling_features.contains(
                        vk::FormatFeatureFlags::SAMPLED_IMAGE
                            | vk::FormatFeatureFlags::TRANSFER_SRC,
                    )
            }) {
                continue;
            }
            let mut drm = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
                .drm_format_modifier(modifier)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let info = vk::PhysicalDeviceImageFormatInfo2::default()
                .format(vk::Format::B8G8R8A8_UNORM)
                .ty(vk::ImageType::TYPE_2D)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
                .push_next(&mut drm)
                .push_next(&mut external);
            let mut external_properties = vk::ExternalImageFormatProperties::default();
            let mut properties =
                vk::ImageFormatProperties2::default().push_next(&mut external_properties);
            // SAFETY: both chains are valid, all handles belong to the guarded device.
            if unsafe {
                instance.get_physical_device_image_format_properties2(
                    physical,
                    &info,
                    &mut properties,
                )
            }
            .is_ok()
                && external_properties
                    .external_memory_properties
                    .external_memory_features
                    .contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE)
            {
                result.push(modifier);
            }
        }
        Ok(result)
    }
}

// Raw allocation lifetime is shared with the hal texture's drop callback. The wgpu device is
// retained until after the image and memory have been freed, including outstanding submissions.
struct Allocation {
    _device: wgpu::Device,
    raw: ash::Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
}
impl Drop for Allocation {
    fn drop(&mut self) {
        // SAFETY: the retained wgpu device outlives these handles; the hal texture's callback
        // holds an allocation reference until submissions finish. Image precedes its memory.
        unsafe {
            self.raw.destroy_image(self.image, None);
            self.raw.free_memory(self.memory, None);
        }
    }
}

pub(crate) struct Export {
    pub(crate) fd: OwnedFd,
    pub(crate) offset: u32,
    pub(crate) stride: u32,
    pub(crate) modifier: u64,
    pub(crate) texture: Option<wgpu::Texture>,
    allocation: Arc<Allocation>,
}

impl Export {
    pub(crate) fn new(
        gpu: &Gpu,
        size: PixelSize,
        modifiers: &[u64],
    ) -> Result<Self, PlatformError> {
        // SAFETY: all Vulkan calls below use this guarded wgpu device and its instance.
        let hal = unsafe { gpu.device.as_hal::<wgpu::hal::api::Vulkan>() }
            .ok_or_else(|| error("Vulkan device unavailable"))?;
        let raw = hal.raw_device();
        let instance = hal.shared_instance().raw_instance();
        let mut list =
            vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(modifiers);
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::B8G8R8A8_UNORM)
            .extent(vk::Extent3D {
                width: size.width,
                height: size.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .push_next(&mut list)
            .push_next(&mut external);
        // SAFETY: modifier support and DMA-BUF export were queried before allocation.
        let image = unsafe { raw.create_image(&info, None) }.map_err(error)?;
        let mut allocation = Allocation {
            _device: gpu.device.clone(),
            raw: raw.clone(),
            image,
            memory: vk::DeviceMemory::null(),
        };
        // SAFETY: image is live and was created on this device.
        let requirements = unsafe { raw.get_image_memory_requirements(image) };
        // SAFETY: physical device belongs to the guarded instance.
        let memory =
            unsafe { instance.get_physical_device_memory_properties(hal.raw_physical_device()) };
        let index = memory
            .memory_types_as_slice()
            .iter()
            .enumerate()
            .find(|(i, ty)| {
                requirements.memory_type_bits & (1 << i) != 0
                    && ty
                        .property_flags
                        .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
            })
            .map(|(i, _)| i as u32)
            .ok_or_else(|| error("no device-local memory"))?;
        let mut export = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(index)
            .push_next(&mut export)
            .push_next(&mut dedicated);
        // SAFETY: memory type satisfies this image; allocation is dedicated to it.
        allocation.memory = unsafe { raw.allocate_memory(&info, None) }.map_err(error)?;
        // SAFETY: dedicated compatible memory at offset zero.
        unsafe { raw.bind_image_memory(image, allocation.memory, 0) }.map_err(error)?;
        let modifier_api = ash::ext::image_drm_format_modifier::Device::new(instance, raw);
        let mut properties = vk::ImageDrmFormatModifierPropertiesEXT::default();
        // SAFETY: the live image uses DRM modifier tiling and the extension is enabled.
        unsafe { modifier_api.get_image_drm_format_modifier_properties(image, &mut properties) }
            .map_err(error)?;
        // SAFETY: queried modifiers are single memory plane; plane zero is valid for this image.
        let layout = unsafe {
            raw.get_image_subresource_layout(
                image,
                vk::ImageSubresource::default()
                    .aspect_mask(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT),
            )
        };
        let offset = u32::try_from(layout.offset).map_err(error)?;
        let stride = u32::try_from(layout.row_pitch).map_err(error)?;
        let fd_api = ash::khr::external_memory_fd::Device::new(instance, raw);
        // SAFETY: this dedicated allocation was created for DMA-BUF export, extension enabled.
        let fd = unsafe {
            fd_api.get_memory_fd(
                &vk::MemoryGetFdInfoKHR::default()
                    .memory(allocation.memory)
                    .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT),
            )
        }
        .map_err(error)?;
        // SAFETY: Vulkan returned a new fd owned by the caller.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Self {
            fd,
            offset,
            stride,
            modifier: properties.drm_format_modifier,
            texture: None,
            allocation: Arc::new(allocation),
        })
    }

    // Called only after ready: the compositor has initialized the entire image and signalled
    // its fence. GENERAL is the external memory layout; every release restores it before reuse.
    pub(crate) fn texture(
        &mut self,
        gpu: &Gpu,
        size: PixelSize,
    ) -> Result<wgpu::Texture, PlatformError> {
        if let Some(texture) = &self.texture {
            return Ok(texture.clone());
        }
        // SAFETY: the allocation was made on this guarded device and ready completed its writes.
        let hal = unsafe { gpu.device.as_hal::<wgpu::hal::api::Vulkan>() }
            .ok_or_else(|| error("Vulkan device unavailable"))?;
        let retained = self.allocation.clone();
        let descriptor = wgpu::hal::TextureDescriptor {
            label: Some("DMA-BUF capture"),
            size: wgpu::Extent3d {
                width: size.width,
                height: size.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUses::RESOURCE | wgpu::TextureUses::COPY_SRC,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
            view_formats: vec![],
        };
        // SAFETY: matching descriptor on the allocating device. External memory is freed by
        // the callback after the last wgpu use; hal must not destroy it itself.
        let texture = unsafe {
            hal.texture_from_raw(
                self.allocation.image,
                &descriptor,
                Some(Box::new(move || drop(retained))),
                wgpu::hal::vulkan::TextureMemory::External,
            )
        };
        drop(hal);
        // SAFETY: ready has initialized the image. The combination of read usages maps to
        // GENERAL in wgpu-hal Vulkan, preserving the external image's contents on first use.
        let texture = unsafe {
            gpu.device
                .create_texture_from_hal::<wgpu::hal::api::Vulkan>(
                    texture,
                    &wgpu::TextureDescriptor {
                        label: Some("DMA-BUF capture"),
                        size: descriptor.size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::Bgra8Unorm,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
                        view_formats: &[],
                    },
                    wgpu::TextureUses::RESOURCE | wgpu::TextureUses::COPY_SRC,
                )
        };
        self.texture = Some(texture.clone());
        Ok(texture)
    }
}

/// The texture of a frame this crate captured into a DMA-BUF, and its crop origin.
/// Returns `None` for other native image implementations.
pub fn texture_of(image: &dyn NativeImage) -> Option<(&wgpu::Texture, (u32, u32))> {
    let image = image.as_any().downcast_ref::<Image>()?;
    Some((&image.texture, image.origin))
}

pub(crate) struct Image {
    pub(crate) texture: wgpu::Texture,
    pub(crate) gpu: Gpu,
    pub(crate) size: PixelSize,
    pub(crate) origin: (u32, u32),
    pub(crate) free: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) modifier: u64,
}
impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DmabufImage")
            .field("size", &self.size)
            .field("origin", &self.origin)
            .field("modifier", &format_args!("{:#018x}", self.modifier))
            .finish_non_exhaustive()
    }
}
impl Drop for Image {
    fn drop(&mut self) {
        let free = self.free.clone();
        let mut encoder = self.gpu.device.create_command_encoder(&Default::default());
        encoder.transition_resources(
            std::iter::empty(),
            [wgpu::TextureTransition {
                texture: &self.texture,
                selector: None,
                state: wgpu::TextureUses::RESOURCE | wgpu::TextureUses::COPY_SRC,
            }]
            .into_iter(),
        );
        self.gpu.queue.submit([encoder.finish()]);
        // Consumers finish submitting while holding the frame. Never recycle a slot before
        // submitted GPU reads complete, even when the frame itself is released early.
        self.gpu
            .queue
            .on_submitted_work_done(move || free.store(true, std::sync::atomic::Ordering::Release));
    }
}
impl NativeImage for Image {
    fn size(&self) -> PixelSize {
        self.size
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn read(&self, f: &mut dyn FnMut(&[u8], u32)) -> Result<(), PlatformError> {
        let stride = self
            .size
            .width
            .checked_mul(4)
            .and_then(|n| n.checked_add(255))
            .map(|n| n / 256 * 256)
            .ok_or_else(|| error("read stride overflow"))?;
        let buffer = self.gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("DMA-BUF CPU fallback"),
            size: u64::from(stride) * u64::from(self.size.height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self.gpu.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: self.origin.0,
                    y: self.origin.1,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride),
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width: self.size.width,
                height: self.size.height,
                depth_or_array_layers: 1,
            },
        );
        let submission = self.gpu.queue.submit([encoder.finish()]);
        let (send, recv) = std::sync::mpsc::channel();
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = send.send(result);
            });
        self.gpu
            .device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(Duration::from_secs(2)),
            })
            .map_err(error)?;
        recv.recv_timeout(Duration::from_secs(2))
            .map_err(error)?
            .map_err(error)?;
        {
            let mapped = buffer.slice(..).get_mapped_range().map_err(error)?;
            f(&mapped, stride);
        }
        buffer.unmap();
        Ok(())
    }
}
