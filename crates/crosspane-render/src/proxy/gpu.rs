//! Shared by window presentation and offscreen byte-for-byte tests.

use crosspane_types::geom::{PixelRect, PixelSize};

#[derive(Debug)]
pub(super) struct Presenter {
    pipeline: wgpu::RenderPipeline,
    canvas: Option<Canvas>,
    texture_format: wgpu::TextureFormat,
    pub(super) grey: f64,
}

#[derive(Debug)]
struct Canvas {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    size: PixelSize,
}

impl Presenter {
    pub(super) fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let grey = if format.is_srgb() {
            ((32.0_f64 / 255.0 + 0.055) / 1.055).powf(2.4)
        } else {
            32.0 / 255.0
        };
        let source = include_str!("../present.wgsl").replace("GREY", &format!("{grey:.16}"));
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("proxy 1:1 presentation"),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("proxy 1:1 presentation"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        Self {
            pipeline,
            canvas: None,
            texture_format: if format.is_srgb() {
                wgpu::TextureFormat::Bgra8UnormSrgb
            } else {
                wgpu::TextureFormat::Bgra8Unorm
            },
            grey,
        }
    }

    pub(super) fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        size: PixelSize,
        pixels: &[u8],
        dirty: &[PixelRect],
    ) -> Result<(), String> {
        let limit = device.limits().max_texture_dimension_2d;
        if size.width == 0 || size.height == 0 || size.width > limit || size.height > limit {
            return Err("canvas dimensions are zero or exceed GPU limits".into());
        }
        let row_bytes = (size.width as usize)
            .checked_mul(4)
            .ok_or("canvas row length overflows")?;
        let length = row_bytes
            .checked_mul(size.height as usize)
            .ok_or("canvas length overflows")?;
        if pixels.len() != length {
            return Err("canvas length does not match its dimensions".into());
        }
        // Validate all damage before modifying the texture; never index unchecked input.
        for rect in dirty {
            if rect.min.x < 0
                || rect.min.y < 0
                || rect.max.x < rect.min.x
                || rect.max.y < rect.min.y
                || rect.max.x as u32 > size.width
                || rect.max.y as u32 > size.height
            {
                return Err("dirty rectangle lies outside the canvas".into());
            }
        }
        if self
            .canvas
            .as_ref()
            .is_none_or(|canvas| canvas.size != size)
        {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("proxy BGRA canvas"),
                size: wgpu::Extent3d {
                    width: size.width,
                    height: size.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: self.texture_format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("proxy canvas"),
                layout: &self.pipeline.get_bind_group_layout(0),
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                }],
            });
            self.canvas = Some(Canvas {
                texture,
                bind_group,
                size,
            });
        }
        let Some(canvas) = &self.canvas else {
            return Err("canvas allocation failed".into());
        };
        for rect in dirty {
            let width = (rect.max.x - rect.min.x) as u32;
            let height = (rect.max.y - rect.min.y) as u32;
            if width == 0 || height == 0 {
                continue;
            }
            let damage_row_bytes = width as usize * 4;
            let mut rows = Vec::with_capacity(damage_row_bytes * height as usize);
            for y in rect.min.y..rect.max.y {
                let start = y as usize * row_bytes + rect.min.x as usize * 4;
                rows.extend_from_slice(&pixels[start..start + damage_row_bytes]);
            }
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &canvas.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: rect.min.x as u32,
                        y: rect.min.y as u32,
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                &rows,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 4),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
        }
        Ok(())
    }

    pub(super) fn encode(&self, encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("proxy presentation"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: self.grey,
                        g: self.grey,
                        b: self.grey,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        if let Some(canvas) = &self.canvas {
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &canvas.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
    }
}

pub(super) fn surface_format(formats: &[wgpu::TextureFormat]) -> Option<wgpu::TextureFormat> {
    // Only 8-bit formats can preserve the supplied BGRA8 bytes. Prefer native BGRA,
    // then RGBA; match the texture's sRGB decoding to the surface's encoding.
    use wgpu::TextureFormat::{Bgra8Unorm, Bgra8UnormSrgb, Rgba8Unorm, Rgba8UnormSrgb};
    [Bgra8Unorm, Rgba8Unorm, Bgra8UnormSrgb, Rgba8UnormSrgb]
        .into_iter()
        .find(|format| formats.contains(format))
}
