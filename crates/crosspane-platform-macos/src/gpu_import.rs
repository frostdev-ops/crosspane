//! Import captured and decoded IOSurfaces without copying pixels.

use std::sync::Arc;

use crosspane_media::picture::NativePicture;
use objc2_core_video::{
    CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane, CVPixelBufferGetIOSurface,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane,
    kCVPixelFormatType_32BGRA,
};
use objc2_metal::{
    MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTextureDescriptor, MTLTextureType,
    MTLTextureUsage,
};
use wgpu::hal::api::Metal;

use crate::frame_capture::SckImage;
use crate::video::VtPicture;

/// A captured frame's IOSurface as a `Bgra8Unorm` texture (`TEXTURE_BINDING`) on `device`, and
/// the frame's top-left corner in it (its crop region). The texture keeps the capture buffer
/// alive until wgpu destroys it. `None` when `image` isn't a frame this crate captured, its buffer
/// isn't IOSurface-backed BGRA, or `device` isn't Metal.
pub fn wrap_capture(
    device: &wgpu::Device,
    image: &dyn crosspane_platform::NativeImage,
) -> Option<(wgpu::Texture, (u32, u32))> {
    let image = image.as_any().downcast_ref::<SckImage>()?;
    if CVPixelBufferGetPixelFormatType(&image.buffer) != kCVPixelFormatType_32BGRA {
        return None;
    }
    let surface = CVPixelBufferGetIOSurface(Some(&image.buffer))?;
    let width = u32::try_from(CVPixelBufferGetWidth(&image.buffer)).ok()?;
    let height = u32::try_from(CVPixelBufferGetHeight(&image.buffer)).ok()?;
    let origin = (
        u32::try_from(image.region.min.x).ok()?,
        u32::try_from(image.region.min.y).ok()?,
    );
    let max = (
        u32::try_from(image.region.max.x).ok()?,
        u32::try_from(image.region.max.y).ok()?,
    );
    if origin.0 >= max.0 || origin.1 >= max.1 || max.0 > width || max.1 > height {
        return None;
    }
    // SAFETY: Only create a resource on this device; never mutate existing wgpu resources.
    let hal = unsafe { device.as_hal::<Metal>() }?;
    // SAFETY: Format and extent match the completed, immutable BGRA IOSurface, plane 0.
    // Shared storage and ShaderRead permit sampling only, never writes to SCK storage.
    let raw = unsafe {
        let desc = MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            MTLPixelFormat::BGRA8Unorm,
            width as usize,
            height as usize,
            false,
        );
        desc.setUsage(MTLTextureUsage::ShaderRead);
        desc.setStorageMode(MTLStorageMode::Shared);
        hal.raw_device()
            .newTextureWithDescriptor_iosurface_plane(&desc, &surface, 0)
    }?;
    let retained = SckImage::new(image.buffer.clone(), image.region);
    // SAFETY: The raw texture belongs to this device and has the declared format/extent.
    // Completed SCK storage is initialized and read-only. The callback owns one buffer retain;
    // wgpu runs it only when destroying the hal texture after its last GPU submission completes.
    let texture = unsafe {
        let raw = wgpu::hal::metal::Device::texture_from_raw(
            raw,
            wgpu::TextureFormat::Bgra8Unorm,
            MTLTextureType::Type2D,
            1,
            1,
            wgpu::hal::CopyExtent {
                width,
                height,
                depth: 1,
            },
            Some(Box::new(move || drop(retained))),
        );
        device.create_texture_from_hal::<Metal>(
            raw,
            &wgpu::TextureDescriptor {
                label: Some("SCK IOSurface"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::TextureUses::RESOURCE,
        )
    };
    Some((texture, origin))
}

/// Wrap a VT picture's luma and chroma planes on the receiving host's Metal device.
pub fn import_picture(
    device: &wgpu::Device,
    picture: &dyn NativePicture,
) -> Result<[wgpu::Texture; 2], String> {
    let picture = picture
        .as_any()
        .downcast_ref::<VtPicture>()
        .ok_or_else(|| "picture is not a VideoToolbox picture".to_owned())?;
    let surface = CVPixelBufferGetIOSurface(Some(&picture.image))
        .ok_or_else(|| "VideoToolbox picture has no IOSurface".to_owned())?;
    // SAFETY: The guard is used only to create textures on this device; no raw resource
    // is destroyed or mutated behind wgpu's back.
    let hal =
        unsafe { device.as_hal::<Metal>() }.ok_or_else(|| "wgpu device is not Metal".to_owned())?;
    // One extra CoreVideo retain shared by both callbacks, including partial failure.
    let retained = Arc::new(VtPicture::retained(picture));
    let mut textures = Vec::with_capacity(2);
    for (plane, (format, metal_format)) in [
        (wgpu::TextureFormat::R8Unorm, MTLPixelFormat::R8Unorm),
        (wgpu::TextureFormat::Rg8Unorm, MTLPixelFormat::RG8Unorm),
    ]
    .into_iter()
    .enumerate()
    {
        let width = CVPixelBufferGetWidthOfPlane(&picture.image, plane);
        let height = CVPixelBufferGetHeightOfPlane(&picture.image, plane);
        if width != picture.size().width as usize / (plane + 1)
            || height != picture.size().height as usize / (plane + 1)
        {
            return Err("VideoToolbox plane dimensions do not match NV12".to_owned());
        }
        // SAFETY: Descriptor format and dimensions match this NV12 IOSurface plane;
        // shared storage and ShaderRead describe immutable sampling of VT's completed output.
        let raw = unsafe {
            let desc =
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    metal_format,
                    width,
                    height,
                    false,
                );
            desc.setUsage(MTLTextureUsage::ShaderRead);
            desc.setStorageMode(MTLStorageMode::Shared);
            hal.raw_device()
                .newTextureWithDescriptor_iosurface_plane(&desc, &surface, plane)
        }
        .ok_or_else(|| format!("Metal refused IOSurface plane {plane}"))?;
        let keep_alive = Arc::clone(&retained);
        // SAFETY: Raw texture belongs to this Metal device, with the stated format/extent.
        // Synchronous VT output is initialized; RESOURCE describes shader-read usage.
        // The shared buffer retain outlives both textures. wgpu destroys each hal texture
        // only after its last submission completes, then runs this callback.
        let texture = unsafe {
            let raw = wgpu::hal::metal::Device::texture_from_raw(
                raw,
                format,
                MTLTextureType::Type2D,
                1,
                1,
                wgpu::hal::CopyExtent {
                    width: width as u32,
                    height: height as u32,
                    depth: 1,
                },
                Some(Box::new(move || drop(keep_alive))),
            );
            device.create_texture_from_hal::<Metal>(
                raw,
                &wgpu::TextureDescriptor {
                    label: Some("VT IOSurface plane"),
                    size: wgpu::Extent3d {
                        width: width as u32,
                        height: height as u32,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::TextureUses::RESOURCE,
            )
        };
        textures.push(texture);
    }
    textures
        .try_into()
        .map_err(|_| "missing NV12 plane".to_owned())
}

#[cfg(test)]
mod capture_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crosspane_types::geom::PixelRect;
    use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString, CFType};
    use objc2_core_video::{
        CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress,
        CVPixelBufferGetBytesPerRow, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
        CVPixelBufferUnlockBaseAddress, kCVPixelBufferIOSurfacePropertiesKey,
        kCVPixelBufferMetalCompatibilityKey,
    };
    use std::{ptr::NonNull, sync::mpsc, time::Duration};

    fn gpu() -> Option<(wgpu::Device, wgpu::Queue)> {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::METAL;
        let instance = wgpu::Instance::new(desc);
        let adapter = match pollster::block_on(instance.request_adapter(&Default::default())) {
            Ok(adapter) => adapter,
            Err(error) => {
                eprintln!("SKIP: headless Metal adapter unavailable: {error}");
                return None;
            }
        };
        Some(pollster::block_on(adapter.request_device(&Default::default())).unwrap())
    }

    fn wait(device: &wgpu::Device) {
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(Duration::from_secs(30)),
            })
            .unwrap();
    }

    fn fixture() -> (CFRetained<CVPixelBuffer>, Vec<u8>) {
        let surface = CFDictionary::<CFString, CFType>::from_slices(&[], &[]);
        // SAFETY: Public immutable attribute keys with the documented dictionary/boolean values.
        let attrs = unsafe {
            CFDictionary::<CFString, CFType>::from_slices(
                &[
                    kCVPixelBufferIOSurfacePropertiesKey,
                    kCVPixelBufferMetalCompatibilityKey,
                ],
                &[surface.as_ref(), CFBoolean::new(true).as_ref()],
            )
        };
        let mut raw = std::ptr::null_mut();
        // SAFETY: Valid BGRA extent, attribute dictionary and writable output pointer.
        assert_eq!(
            // SAFETY: Valid BGRA extent, attribute dictionary and writable output pointer.
            unsafe {
                CVPixelBufferCreate(
                    None,
                    66,
                    34,
                    kCVPixelFormatType_32BGRA,
                    Some(attrs.as_opaque()),
                    NonNull::from(&mut raw),
                )
            },
            0
        );
        // SAFETY: Successful Create transfers one owned reference.
        let buffer = unsafe { CFRetained::from_raw(NonNull::new(raw).unwrap()) };
        assert!(CVPixelBufferGetIOSurface(Some(&buffer)).is_some());
        let pattern: Vec<u8> = (0..66 * 34 * 4).map(|i| (i % 256) as u8).collect();
        // SAFETY: Exclusive fixture ownership, balanced write lock/unlock.
        assert_eq!(
            // SAFETY: Exclusive fixture ownership, balanced write lock/unlock below.
            unsafe { CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags::empty()) },
            0
        );
        let stride = CVPixelBufferGetBytesPerRow(&buffer);
        let base = CVPixelBufferGetBaseAddress(&buffer).cast::<u8>();
        assert!(!base.is_null());
        assert!(stride >= 66 * 4);
        // SAFETY: Each copy is within a locked row and the corresponding packed pattern row.
        unsafe {
            for y in 0..34 {
                std::ptr::copy_nonoverlapping(
                    pattern.as_ptr().add(y * 66 * 4),
                    base.add(y * stride),
                    66 * 4,
                );
            }
            assert_eq!(
                CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags::empty()),
                0
            );
        }
        (buffer, pattern)
    }

    // Imported textures deliberately have only TEXTURE_BINDING. textureLoad reads their exact
    // samples into a storage buffer, avoiding an invalid COPY_SRC use or changing production usage.
    fn read_bgra(device: &wgpu::Device, queue: &wgpu::Queue, texture: wgpu::Texture) -> Vec<u8> {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: None,
            source: wgpu::ShaderSource::Wgsl(
                r#"
            @group(0) @binding(0) var plane: texture_2d<f32>;
            @group(0) @binding(1) var<storage, read_write> output: array<u32>;
            @compute @workgroup_size(8,8)
            fn main(@builtin(global_invocation_id) id: vec3<u32>) {
                let size = textureDimensions(plane);
                if (id.x >= size.x || id.y >= size.y) { return; }
                let value = vec4<u32>(round(textureLoad(plane, vec2<i32>(id.xy), 0).bgra * 255.0));
                let offset = (id.y * size.x + id.x) * 4u;
                output[offset] = value.x;
                output[offset + 1u] = value.y;
                output[offset + 2u] = value.z;
                output[offset + 3u] = value.w;
            }"#
                .into(),
            ),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: None,
            layout: None,
            module: &shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        let bytes = u64::from(texture.width()) * u64::from(texture.height()) * 16;
        let output = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: bytes,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: bytes,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let view = texture.create_view(&Default::default());
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: output.as_entire_binding(),
                },
            ],
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &group, &[]);
            pass.dispatch_workgroups(texture.width().div_ceil(8), texture.height().div_ceil(8), 1);
        }
        encoder.copy_buffer_to_buffer(&output, 0, &readback, 0, bytes);
        queue.submit([encoder.finish()]);
        // Release caller ownership while the submission may still be reading the IOSurface.
        drop(group);
        drop(view);
        drop(texture);
        let (tx, rx) = mpsc::channel();
        readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                tx.send(result).unwrap();
            });
        wait(device);
        rx.recv_timeout(Duration::from_secs(30)).unwrap().unwrap();
        let mapped = readback.slice(..).get_mapped_range().unwrap();
        let values = mapped
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| u32::from_ne_bytes(*bytes) as u8)
            .collect();
        drop(mapped);
        readback.unmap();
        values
    }

    #[test]
    fn capture_byte_exact_and_crop_origin() {
        let Some((device, queue)) = gpu() else {
            return;
        };
        let baseline = crate::frame_capture::held_capture_buffers();
        let (buffer, pattern) = fixture();
        let image = SckImage::new(
            buffer.clone(),
            PixelRect::new([3, 5].into(), [63, 31].into()),
        );
        let before = buffer.retain_count();
        let (texture, origin) =
            wrap_capture(&device, &image).expect("Metal refused SCK-style BGRA IOSurface");
        assert_eq!(origin, (3, 5));
        assert_eq!((texture.width(), texture.height()), (66, 34));
        assert_eq!(texture.format(), wgpu::TextureFormat::Bgra8Unorm);
        assert_eq!(texture.usage(), wgpu::TextureUsages::TEXTURE_BINDING);
        assert!(buffer.retain_count() > before);
        // Drop caller ownership before GPU readback; the callback must keep storage alive.
        drop(image);
        assert_eq!(read_bgra(&device, &queue, texture), pattern);
        wait(&device);
        assert_eq!(buffer.retain_count(), before - 1);
        assert_eq!(crate::frame_capture::held_capture_buffers(), baseline);
    }

    fn resident_bytes() -> u64 {
        let output = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse::<u64>()
            .unwrap()
            * 1024
    }

    #[test]
    fn capture_release_1000_cycles() {
        let Some((device, queue)) = gpu() else {
            return;
        };
        let baseline = crate::frame_capture::held_capture_buffers();
        let (buffer, pattern) = fixture();
        let before_retain = buffer.retain_count();
        let region = PixelRect::new([3, 5].into(), [63, 31].into());
        // Warm both the import and submission paths before measuring residency.
        for _ in 0..20 {
            let image = SckImage::new(buffer.clone(), region);
            let (texture, _) =
                wrap_capture(&device, &image).expect("Metal refused SCK-style BGRA IOSurface");
            drop(image);
            drop(texture);
            wait(&device);
        }
        let image = SckImage::new(buffer.clone(), region);
        let (texture, _) = wrap_capture(&device, &image).unwrap();
        drop(image);
        assert_eq!(read_bgra(&device, &queue, texture), pattern);
        wait(&device);
        let before = resident_bytes();
        for _ in 0..1000 {
            let image = Arc::new(SckImage::new(buffer.clone(), region));
            let (texture, _) = wrap_capture(&device, image.as_ref()).unwrap();
            assert_eq!(Arc::strong_count(&image), 1);
            drop(image);
            drop(texture);
            wait(&device);
            assert_eq!(buffer.retain_count(), before_retain);
            assert_eq!(crate::frame_capture::held_capture_buffers(), baseline);
        }
        let growth = resident_bytes().saturating_sub(before);
        eprintln!("capture resident growth over 1000 cycles: {growth} bytes");
        assert!(growth <= 32 * 1024 * 1024);
    }
}
