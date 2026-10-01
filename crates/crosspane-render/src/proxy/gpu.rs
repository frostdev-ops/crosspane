//! Shared by window presentation and offscreen byte-for-byte tests.

use std::{cell::Cell, sync::Arc};

use crosspane_media::picture::Nv12;

use crosspane_types::geom::{PixelRect, PixelSize};

#[derive(Debug)]
pub(super) struct Presenter {
    pipeline: wgpu::RenderPipeline,
    canvas: Option<Canvas>,
    video_pipeline: wgpu::RenderPipeline,
    video: Option<Video>,
    pending_video: Option<(PixelSize, Arc<Nv12>)>,
    video_visible: bool,
    #[cfg(test)]
    pub(super) video_uploads: usize,
    video_uniform: wgpu::Buffer,
    fallback: wgpu::BindGroup,
    uniform: wgpu::Buffer,
    last_uniform: Cell<Option<[u32; 8]>>,
    accent: [u8; 3],
    edge_px: u32,
    srgb: bool,
    texture_format: wgpu::TextureFormat,
    pub(super) grey: f64,
}

#[derive(Debug)]
struct Canvas {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    size: PixelSize,
}

#[derive(Debug)]
struct Video {
    y: wgpu::Texture,
    uv: wgpu::Texture,
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
        let video_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("proxy NV12 presentation"),
            source: wgpu::ShaderSource::Wgsl(
                include_str!("../video.wgsl")
                    .replace("GREY", &format!("{grey:.16}"))
                    .replace("SRGB", if format.is_srgb() { "true" } else { "false" })
                    .into(),
            ),
        });
        let video_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("proxy NV12 presentation"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &video_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &video_shader,
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
        let video_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("proxy YUV conversion"),
            size: 80,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("proxy edge"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let empty = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("proxy empty canvas"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let fallback = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("proxy empty canvas"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(
                        &empty.create_view(&Default::default()),
                    ),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: uniform.as_entire_binding(),
                },
            ],
        });
        Self {
            pipeline,
            video_pipeline,
            video_uniform,
            video: None,
            pending_video: None,
            video_visible: false,
            #[cfg(test)]
            video_uploads: 0,
            fallback,
            uniform,
            last_uniform: Cell::new(None),
            accent: [0; 3],
            edge_px: 0,
            srgb: format.is_srgb(),
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
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: self.uniform.as_entire_binding(),
                    },
                ],
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
                pixels,
                wgpu::TexelCopyBufferLayout {
                    offset: (rect.min.y as usize * row_bytes + rect.min.x as usize * 4) as u64,
                    bytes_per_row: Some(size.width * 4),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
        }
        self.pending_video = None;
        self.video_visible = false;
        Ok(())
    }

    /// Validation happens on arrival, but only the newest pending picture reaches the GPU.
    pub(super) fn set_video(
        &mut self,
        device: &wgpu::Device,
        size: PixelSize,
        picture: Arc<Nv12>,
    ) -> Result<(), String> {
        picture.validate().map_err(|error| error.to_string())?;
        let limit = device.limits().max_texture_dimension_2d;
        if size.width == 0
            || size.height == 0
            || size.width > picture.size.width
            || size.height > picture.size.height
            || picture.size.width > limit
            || picture.size.height > limit
        {
            return Err(
                "video dimensions are zero, outside the picture or exceed GPU limits".into(),
            );
        }
        self.pending_video = Some((size, picture));
        Ok(())
    }

    pub(super) fn prepare_video(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        let Some((visible, picture)) = self.pending_video.take() else {
            return;
        };
        if self
            .video
            .as_ref()
            .is_none_or(|video| video.size != picture.size)
        {
            let plane = |size: PixelSize, format| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("proxy NV12 plane"),
                    size: wgpu::Extent3d {
                        width: size.width,
                        height: size.height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                })
            };
            let y = plane(picture.size, wgpu::TextureFormat::R8Unorm);
            let uv = plane(
                PixelSize::new(picture.size.width / 2, picture.size.height / 2),
                wgpu::TextureFormat::Rg8Unorm,
            );
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("proxy NV12"),
                layout: &self.video_pipeline.get_bind_group_layout(0),
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(
                            &y.create_view(&Default::default()),
                        ),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: self.uniform.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(
                            &uv.create_view(&Default::default()),
                        ),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: self.video_uniform.as_entire_binding(),
                    },
                ],
            });
            self.video = Some(Video {
                y,
                uv,
                bind_group,
                size: picture.size,
            });
        }
        let Some(video) = &self.video else {
            return;
        };
        for (texture, bytes, stride) in [
            (&video.y, &picture.y, picture.y_stride),
            (&video.uv, &picture.uv, picture.uv_stride),
        ] {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                bytes,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride),
                    rows_per_image: None,
                },
                texture.size(),
            );
        }
        // Four padded vec3<f32> values, followed by the visible dimensions.
        let conversion = picture.colour.to_rgb();
        let mut data = [0_u32; 20];
        for (slots, values) in data[..16]
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(std::iter::once(conversion.offset).chain(conversion.rows))
        {
            for (slot, value) in slots.iter_mut().zip(values) {
                *slot = value.to_bits();
            }
        }
        data[16] = visible.width;
        data[17] = visible.height;
        let bytes: Vec<u8> = data.iter().flat_map(|value| value.to_ne_bytes()).collect();
        queue.write_buffer(&self.video_uniform, 0, &bytes);
        self.video_visible = true;
        #[cfg(test)]
        {
            self.video_uploads += 1;
        }
    }

    pub(super) fn set_edge(&mut self, accent: [u8; 3], edge_px: u32) {
        self.accent = accent;
        self.edge_px = edge_px;
    }

    pub(super) fn encode(
        &self,
        queue: &wgpu::Queue,
        size: PixelSize,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
    ) {
        // vec4<f32> colour followed by vec4<u32> (width, height, edge, canvas-present).
        let mut data = [0_u32; 8];
        for (slot, byte) in data[..3].iter_mut().zip(self.accent) {
            let value = f32::from(byte) / 255.0;
            let value = if self.srgb {
                if value <= 0.04045 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            } else {
                value
            };
            *slot = value.to_bits();
        }
        data[3] = 1.0_f32.to_bits();
        data[4..].copy_from_slice(&[
            size.width,
            size.height,
            self.edge_px,
            u32::from(self.canvas.is_some()),
        ]);
        if self.last_uniform.get() != Some(data) {
            let bytes: Vec<u8> = data.iter().flat_map(|value| value.to_ne_bytes()).collect();
            queue.write_buffer(&self.uniform, 0, &bytes);
            self.last_uniform.set(Some(data));
        }
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
        let video = self.video.as_ref().filter(|_| self.video_visible);
        pass.set_pipeline(if video.is_some() {
            &self.video_pipeline
        } else {
            &self.pipeline
        });
        pass.set_bind_group(
            0,
            video.map_or_else(
                || {
                    self.canvas
                        .as_ref()
                        .map_or(&self.fallback, |canvas| &canvas.bind_group)
                },
                |video| &video.bind_group,
            ),
            &[],
        );
        pass.draw(0..3, 0..1);
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
