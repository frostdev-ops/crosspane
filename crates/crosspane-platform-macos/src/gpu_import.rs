//! Import decoded VideoToolbox IOSurface planes without copying pixels.

use std::sync::Arc;

use crosspane_media::picture::NativePicture;
use objc2_core_video::{
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetIOSurface, CVPixelBufferGetWidthOfPlane,
};
use objc2_metal::{
    MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTextureDescriptor, MTLTextureType,
    MTLTextureUsage,
};
use wgpu::hal::api::Metal;

use crate::video::VtPicture;

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
