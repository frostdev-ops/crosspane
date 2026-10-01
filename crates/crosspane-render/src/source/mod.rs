//! GPU work on captured frames at the source (GPU-v0).
use crosspane_media::{picture::YuvColour, tiles::TileSource};
use crosspane_types::geom::PixelSize;
use std::{
    marker::PhantomData,
    sync::mpsc,
    time::{Duration, Instant},
};

/// A byte-preserving BGRA8 region of a sampled GPU texture.
#[derive(Clone, Copy, Debug)]
pub struct FrameRegion<'a> {
    pub texture: &'a wgpu::Texture,
    pub origin: (u32, u32),
    pub size: PixelSize,
}
/// NV12 destination and encoder colour description.
#[derive(Clone, Copy, Debug)]
pub struct Nv12Output<'a> {
    pub target: Nv12Target<'a>,
    pub colour: YuvColour,
}
/// Storage planes or a word-aligned NV12 storage buffer.
#[derive(Clone, Copy, Debug)]
pub enum Nv12Target<'a> {
    Textures {
        y: &'a wgpu::Texture,
        uv: &'a wgpu::Texture,
    },
    Buffer {
        buffer: &'a wgpu::Buffer,
        y_offset: u64,
        y_pitch: u32,
        uv_offset: u64,
        uv_pitch: u32,
    },
}
/// Row-major change bitmap; unused high bits are zero.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileChanges {
    pub tiles_x: u32,
    pub tiles_y: u32,
    pub changed_bits: Vec<u32>,
    pub changed: u32,
}
/// Mapped, tightly packed BGRA tiles. Dropping releases the mapping.
#[derive(Debug)]
pub struct GatheredTiles<'a> {
    view: Option<wgpu::BufferView>,
    buffer: &'a wgpu::Buffer,
    slots: &'a [u32],
    size: PixelSize,
    _borrow: PhantomData<&'a mut SourceGpu>,
}
impl TileSource for GatheredTiles<'_> {
    fn tile(&self, tx: u32, ty: u32) -> Option<&[u8]> {
        let nx = self.size.width.div_ceil(64);
        if tx >= nx || ty >= self.size.height.div_ceil(64) {
            return None;
        }
        let slot = *self.slots.get((ty * nx + tx) as usize)?;
        if slot == u32::MAX {
            return None;
        }
        let len = (self.size.width - tx * 64).min(64) as usize
            * (self.size.height - ty * 64).min(64) as usize
            * 4;
        self.view
            .as_ref()?
            .get(slot as usize * 16384..slot as usize * 16384 + len)
    }
}
impl Drop for GatheredTiles<'_> {
    fn drop(&mut self) {
        self.view.take();
        self.buffer.unmap();
    }
}
#[derive(Debug, thiserror::Error)]
pub enum SourceGpuError {
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    #[error("bad input: {0}")]
    BadInput(&'static str),
    #[error("GPU: {0}")]
    Gpu(String),
}
use SourceGpuError::{BadInput, Gpu, Unsupported};

#[derive(Debug)]
struct Storage {
    buffer: wgpu::Buffer,
    usage: wgpu::BufferUsages,
}
impl Storage {
    fn new(device: &wgpu::Device, usage: wgpu::BufferUsages) -> Self {
        Self {
            buffer: make_buffer(device, 4, usage),
            usage,
        }
    }
    fn grow(&mut self, device: &wgpu::Device, size: u64) {
        if size > self.buffer.size() {
            self.buffer = make_buffer(device, size, self.usage);
        }
    }
}
fn make_buffer(device: &wgpu::Device, size: u64, usage: wgpu::BufferUsages) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("source GPU"),
        size,
        usage,
        mapped_at_creation: false,
    })
}
fn words<const N: usize>(values: &[u32; N]) -> [[u8; 4]; N] {
    values.map(u32::to_le_bytes)
}
fn pipeline(device: &wgpu::Device, code: &str) -> wgpu::ComputePipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("source GPU"),
        source: wgpu::ShaderSource::Wgsl(code.into()),
    });
    device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("source GPU"),
        layout: None,
        module: &module,
        entry_point: Some("main"),
        compilation_options: Default::default(),
        cache: None,
    })
}
// Catch all wgpu error classes rather than allowing the default panic handler.
struct ErrorScopes([Option<wgpu::ErrorScopeGuard>; 3]);
impl ErrorScopes {
    fn new(device: &wgpu::Device) -> Self {
        Self([
            Some(device.push_error_scope(wgpu::ErrorFilter::OutOfMemory)),
            Some(device.push_error_scope(wgpu::ErrorFilter::Internal)),
            Some(device.push_error_scope(wgpu::ErrorFilter::Validation)),
        ])
    }
}
impl Drop for ErrorScopes {
    fn drop(&mut self) {
        for scope in self.0.iter_mut().rev() {
            scope.take();
        }
    }
}
fn scope_result(mut scopes: ErrorScopes) -> Result<(), SourceGpuError> {
    let mut error = None;
    for scope in scopes.0.iter_mut().rev() {
        if let Some(scope) = scope.take() {
            // Native wgpu error-scope futures are immediately ready (no GPU wait).
            if let Some(e) = pollster::block_on(scope.pop()) {
                error = Some(Gpu(e.to_string()));
            }
        }
    }
    match error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
/// Per-device compute pipelines and committed hashes for one capture stream.
#[derive(Debug)]
pub struct SourceGpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    hash: wgpu::ComputePipeline,
    gather_pipeline: wgpu::ComputePipeline,
    nv_buffer: wgpu::ComputePipeline,
    nv_textures: Option<wgpu::ComputePipeline>,
    params: wgpu::Buffer,
    nv_params: wgpu::Buffer,
    hashes: [Storage; 2],
    work_hashes: Storage,
    committed: usize,
    committed_size: Option<PixelSize>,
    scanned_size: Option<PixelSize>,
    bits: Storage,
    bitmap_read: Storage,
    indices: Storage,
    pixels: Storage,
    pixel_read: Storage,
    slots: Vec<u32>,
    slot_bytes: Vec<u8>,
}
impl SourceGpu {
    /// Optional format support; hash and gather require no optional features.
    pub fn optional_features(adapter: &wgpu::Adapter) -> wgpu::Features {
        let f = wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES;
        if adapter.features().contains(f)
            && [wgpu::TextureFormat::R8Unorm, wgpu::TextureFormat::Rg8Unorm]
                .iter()
                .all(|format| {
                    adapter
                        .get_texture_format_features(*format)
                        .allowed_usages
                        .contains(wgpu::TextureUsages::STORAGE_BINDING)
                })
        {
            f
        } else {
            wgpu::Features::empty()
        }
    }
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Result<Self, SourceGpuError> {
        let limits = device.limits();
        if limits.max_compute_invocations_per_workgroup < 64
            || limits.max_compute_workgroup_size_x < 64
            || limits.max_compute_workgroup_size_y < 8
            || limits.max_compute_workgroup_storage_size < 512
            || limits.max_compute_workgroups_per_dimension == 0
            || limits.max_storage_buffers_per_shader_stage < 3
            || limits.max_sampled_textures_per_shader_stage == 0
            || limits.max_uniform_buffers_per_shader_stage == 0
            || limits.max_uniform_buffer_binding_size < 96
            || limits.max_storage_buffer_binding_size < 8
        {
            return Err(Unsupported("insufficient compute limits"));
        }
        let scope = ErrorScopes::new(&device);
        let hash = pipeline(&device, include_str!("hash.wgsl"));
        let gather_pipeline = pipeline(&device, include_str!("gather.wgsl"));
        let nv_buffer = pipeline(&device, include_str!("nv12.wgsl"));
        let storage = wgpu::BufferUsages::STORAGE
            | wgpu::BufferUsages::COPY_DST
            | wgpu::BufferUsages::COPY_SRC;
        let read = wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST;
        let params = make_buffer(
            &device,
            32,
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        );
        let nv_params = make_buffer(
            &device,
            96,
            wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        );
        let hashes = [
            Storage::new(&device, storage),
            Storage::new(&device, storage),
        ];
        let work_hashes = Storage::new(&device, storage);
        let bits = Storage::new(&device, storage);
        let bitmap_read = Storage::new(&device, read);
        let indices = Storage::new(&device, storage);
        let pixels = Storage::new(&device, storage);
        let pixel_read = Storage::new(&device, read);
        scope_result(scope)?;
        let nv_textures = if device
            .features()
            .contains(wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES)
        {
            let scope = ErrorScopes::new(&device);
            let common = include_str!("nv12.wgsl")
                .split("@group(0) @binding(2)")
                .next()
                .ok_or(Unsupported("missing NV12 shader"))?;
            let candidate = pipeline(
                &device,
                &format!("{common}\n{}", include_str!("nv12_textures.wgsl")),
            );
            if scope_result(scope).is_ok() {
                Some(candidate)
            } else {
                None
            }
        } else {
            None
        };
        Ok(Self {
            device,
            queue,
            hash,
            gather_pipeline,
            nv_buffer,
            nv_textures,
            params,
            nv_params,
            hashes,
            work_hashes,
            committed: 0,
            committed_size: None,
            scanned_size: None,
            bits,
            bitmap_read,
            indices,
            pixels,
            pixel_read,
            slots: Vec::new(),
            slot_bytes: Vec::new(),
        })
    }
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }
    pub fn nv12_textures_supported(&self) -> bool {
        self.nv_textures.is_some()
    }
    fn validate(&self, frame: FrameRegion<'_>) -> Result<(u32, u32), SourceGpuError> {
        let t = frame.texture;
        if t.format() != wgpu::TextureFormat::Bgra8Unorm
            || !t.usage().contains(wgpu::TextureUsages::TEXTURE_BINDING)
            || t.dimension() != wgpu::TextureDimension::D2
            || t.depth_or_array_layers() != 1
            || t.sample_count() != 1
        {
            return Err(BadInput(
                "frame must be a sampled, single-layer BGRA8 2D texture",
            ));
        }
        if frame.size.width == 0
            || frame.size.height == 0
            || frame
                .origin
                .0
                .checked_add(frame.size.width)
                .is_none_or(|v| v > t.width())
            || frame
                .origin
                .1
                .checked_add(frame.size.height)
                .is_none_or(|v| v > t.height())
        {
            return Err(BadInput("empty frame or region outside texture"));
        }
        let grid = (
            frame.size.width.div_ceil(64),
            frame.size.height.div_ceil(64),
        );
        let limits = self.device.limits();
        let count = u64::from(grid.0) * u64::from(grid.1);
        if grid.0 > limits.max_compute_workgroups_per_dimension
            || grid.1 > limits.max_compute_workgroups_per_dimension
            || count * 8 > limits.max_storage_buffer_binding_size
        {
            return Err(Unsupported("frame exceeds compute limits"));
        }
        Ok(grid)
    }
    fn write_params(&self, frame: FrameRegion<'_>, grid: (u32, u32), full: bool) {
        self.queue.write_buffer(
            &self.params,
            0,
            words(&[
                frame.origin.0,
                frame.origin.1,
                frame.size.width,
                frame.size.height,
                grid.0,
                grid.1,
                u32::from(full),
                0,
            ])
            .as_flattened(),
        );
    }
    /// Synchronously hash and compare, optionally writing NV12 in the same submission.
    pub fn scan(
        &mut self,
        frame: FrameRegion<'_>,
        nv12: Option<Nv12Output<'_>>,
        full: bool,
    ) -> Result<TileChanges, SourceGpuError> {
        let committed = self.hashes[self.committed].buffer.clone();
        let result = catch_gpu(|| self.scan_inner(frame, nv12, full));
        if result.is_err() {
            self.hashes[self.committed].buffer = committed;
        }
        result
    }
    fn scan_inner(
        &mut self,
        frame: FrameRegion<'_>,
        nv12: Option<Nv12Output<'_>>,
        full: bool,
    ) -> Result<TileChanges, SourceGpuError> {
        let grid = self.validate(frame)?;
        if let Some(output) = nv12 {
            self.validate_nv(frame.size, output)?;
        }
        let count = u64::from(grid.0) * u64::from(grid.1);
        let bytes = count.div_ceil(32) * 4;
        let scope = ErrorScopes::new(&self.device);
        // Keep the last successful scan intact until this submission succeeds.
        let mut encoder = self.device.create_command_encoder(&Default::default());
        let old_committed = if count * 8 > self.hashes[self.committed].buffer.size() {
            let old = self.hashes[self.committed].buffer.clone();
            self.hashes[self.committed].grow(&self.device, count * 8);
            encoder.copy_buffer_to_buffer(
                &old,
                0,
                &self.hashes[self.committed].buffer,
                0,
                old.size(),
            );
            Some(old)
        } else {
            None
        };
        self.work_hashes.grow(&self.device, count * 8);
        self.bits.grow(&self.device, bytes);
        self.bitmap_read.grow(&self.device, bytes);
        self.write_params(frame, grid, full || self.committed_size != Some(frame.size));
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor {
            mip_level_count: Some(1),
            ..Default::default()
        });
        let group = bind(
            &self.device,
            &self.hash,
            &[
                wgpu::BindingResource::TextureView(&view),
                self.params.as_entire_binding(),
                self.hashes[self.committed].buffer.as_entire_binding(),
                self.work_hashes.buffer.as_entire_binding(),
                self.bits.buffer.as_entire_binding(),
            ],
        );
        encoder.clear_buffer(&self.bits.buffer, 0, None);
        dispatch(&mut encoder, &self.hash, &group, grid.0, grid.1);
        if let Some(output) = nv12 {
            self.encode_nv(&mut encoder, &view, frame, output)?;
        }
        encoder.copy_buffer_to_buffer(&self.bits.buffer, 0, &self.bitmap_read.buffer, 0, bytes);
        let submission = self.queue.submit([encoder.finish()]);
        let result = map(&self.device, &self.bitmap_read.buffer, bytes, submission);
        let validation = scope_result(scope);
        let view = match result {
            Ok(view) => view,
            Err(error) => {
                if let Some(old) = old_committed {
                    self.hashes[self.committed].buffer = old;
                }
                return Err(error);
            }
        };
        if let Err(error) = validation {
            drop(view);
            self.bitmap_read.buffer.unmap();
            if let Some(old) = old_committed {
                self.hashes[self.committed].buffer = old;
            }
            return Err(error);
        }
        let changed_bits: Vec<u32> = view
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        drop(view);
        self.bitmap_read.buffer.unmap();
        std::mem::swap(&mut self.work_hashes, &mut self.hashes[1 - self.committed]);
        self.scanned_size = Some(frame.size);
        Ok(TileChanges {
            tiles_x: grid.0,
            tiles_y: grid.1,
            changed: changed_bits.iter().map(|v| v.count_ones()).sum(),
            changed_bits,
        })
    }
    /// Commit the most recent successful scan without GPU work.
    pub fn commit(&mut self) {
        if let Some(size) = self.scanned_size.take() {
            self.committed = 1 - self.committed;
            self.committed_size = Some(size);
        }
    }
    /// Invalidate committed hashes.
    pub fn reset(&mut self) {
        self.committed_size = None;
    }
    /// Gather selected tiles into a reusable mapped buffer.
    pub fn gather<'a>(
        &'a mut self,
        frame: FrameRegion<'_>,
        changes: &TileChanges,
        all: bool,
    ) -> Result<GatheredTiles<'a>, SourceGpuError> {
        catch_gpu(move || self.gather_inner(frame, changes, all))
    }
    fn gather_inner<'a>(
        &'a mut self,
        frame: FrameRegion<'_>,
        changes: &TileChanges,
        all: bool,
    ) -> Result<GatheredTiles<'a>, SourceGpuError> {
        let grid = self.validate(frame)?;
        let count = grid.0 * grid.1;
        if changes.tiles_x != grid.0
            || changes.tiles_y != grid.1
            || changes.changed_bits.len() != count.div_ceil(32) as usize
            || (count % 32 != 0
                && changes
                    .changed_bits
                    .last()
                    .is_some_and(|v| v >> (count % 32) != 0))
            || changes.changed
                != changes
                    .changed_bits
                    .iter()
                    .map(|v| v.count_ones())
                    .sum::<u32>()
        {
            return Err(BadInput("invalid change bitmap or tile grid"));
        }
        self.slots.clear();
        let mut selected = 0u32;
        for i in 0..count {
            if all || changes.changed_bits[(i / 32) as usize] & (1 << (i % 32)) != 0 {
                self.slots.push(selected);
                selected += 1;
            } else {
                self.slots.push(u32::MAX);
            }
        }
        let bytes = (u64::from(selected) * 16384).max(4);
        if bytes > self.device.limits().max_storage_buffer_binding_size
            || bytes > self.device.limits().max_buffer_size
        {
            return Err(Unsupported("gather exceeds buffer limits"));
        }
        let scope = ErrorScopes::new(&self.device);
        self.indices.grow(&self.device, u64::from(count) * 4);
        self.pixels.grow(&self.device, bytes);
        self.pixel_read.grow(&self.device, bytes);
        self.slot_bytes.clear();
        self.slot_bytes
            .extend(self.slots.iter().flat_map(|v| v.to_le_bytes()));
        self.queue
            .write_buffer(&self.indices.buffer, 0, &self.slot_bytes);
        self.write_params(frame, grid, false);
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor {
            mip_level_count: Some(1),
            ..Default::default()
        });
        let group = bind(
            &self.device,
            &self.gather_pipeline,
            &[
                wgpu::BindingResource::TextureView(&view),
                self.params.as_entire_binding(),
                self.indices.buffer.as_entire_binding(),
                self.pixels.buffer.as_entire_binding(),
            ],
        );
        let mut encoder = self.device.create_command_encoder(&Default::default());
        dispatch(&mut encoder, &self.gather_pipeline, &group, grid.0, grid.1);
        encoder.copy_buffer_to_buffer(&self.pixels.buffer, 0, &self.pixel_read.buffer, 0, bytes);
        let submission = self.queue.submit([encoder.finish()]);
        let result = map(&self.device, &self.pixel_read.buffer, bytes, submission);
        let validation = scope_result(scope);
        let view = result?;
        if let Err(e) = validation {
            drop(view);
            self.pixel_read.buffer.unmap();
            return Err(e);
        }
        Ok(GatheredTiles {
            view: Some(view),
            buffer: &self.pixel_read.buffer,
            slots: &self.slots,
            size: frame.size,
            _borrow: PhantomData,
        })
    }
    fn validate_nv(&self, size: PixelSize, output: Nv12Output<'_>) -> Result<(), SourceGpuError> {
        let w = size.width.next_multiple_of(2);
        let h = size.height.next_multiple_of(2);
        match output.target {
            Nv12Target::Textures { y, uv } => {
                if !self.nv12_textures_supported() {
                    return Err(Unsupported("R8/RG8 storage writes"));
                }
                if y.width() % 2 != 0
                    || y.height() % 2 != 0
                    || uv.width() != y.width() / 2
                    || uv.height() != y.height() / 2
                {
                    return Err(BadInput(
                        "NV12 chroma plane must be half the luma dimensions",
                    ));
                }
                for (t, format, width, height) in [
                    (y, wgpu::TextureFormat::R8Unorm, w, h),
                    (uv, wgpu::TextureFormat::Rg8Unorm, w / 2, h / 2),
                ] {
                    if t.format() != format
                        || !t.usage().contains(wgpu::TextureUsages::STORAGE_BINDING)
                        || t.width() < width
                        || t.height() < height
                        || t.dimension() != wgpu::TextureDimension::D2
                        || t.depth_or_array_layers() != 1
                        || t.sample_count() != 1
                    {
                        return Err(BadInput("invalid NV12 storage plane"));
                    }
                }
            }
            Nv12Target::Buffer { .. } => {
                self.buffer_layout(size, output.target)?;
            }
        }
        Ok(())
    }
    fn buffer_layout(
        &self,
        size: PixelSize,
        target: Nv12Target<'_>,
    ) -> Result<(u64, u64, [u32; 2]), SourceGpuError> {
        let Nv12Target::Buffer {
            buffer,
            y_offset,
            y_pitch,
            uv_offset,
            uv_pitch,
        } = target
        else {
            return Err(BadInput("expected NV12 buffer"));
        };
        let w = size.width.next_multiple_of(2);
        let h = size.height.next_multiple_of(2);
        let end = |offset: u64, pitch: u32, rows: u32| {
            offset
                .checked_add(u64::from(pitch) * u64::from(rows - 1))
                .and_then(|v| v.checked_add(u64::from(w.next_multiple_of(4))))
        };
        let ye = end(y_offset, y_pitch, h).ok_or(BadInput("NV12 offset overflow"))?;
        let ue = end(uv_offset, uv_pitch, h / 2).ok_or(BadInput("NV12 offset overflow"))?;
        if !buffer.usage().contains(wgpu::BufferUsages::STORAGE)
            || y_offset % 4 != 0
            || uv_offset % 4 != 0
            || y_pitch % 4 != 0
            || uv_pitch % 4 != 0
            || y_pitch < w
            || uv_pitch < w
            || ye > buffer.size()
            || ue > buffer.size()
            || !(ye <= uv_offset || ue <= y_offset)
        {
            return Err(BadInput("invalid NV12 storage buffer layout"));
        }
        // Bind just the plane span: the caller's u64 offsets may refer into a large pool.
        let alignment = u64::from(self.device.limits().min_storage_buffer_offset_alignment).max(4);
        let base = y_offset.min(uv_offset) / alignment * alignment;
        let length = ye.max(ue) - base;
        if length > self.device.limits().max_storage_buffer_binding_size
            || length / 4 > u64::from(u32::MAX)
        {
            return Err(Unsupported(
                "NV12 plane span exceeds storage binding limits",
            ));
        }
        Ok((
            base,
            length,
            [
                ((y_offset - base) / 4) as u32,
                ((uv_offset - base) / 4) as u32,
            ],
        ))
    }
    fn encode_nv(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        frame: FrameRegion<'_>,
        output: Nv12Output<'_>,
    ) -> Result<(), SourceGpuError> {
        let w = frame.size.width.next_multiple_of(2);
        let h = frame.size.height.next_multiple_of(2);
        let (offsets, pitches) = match output.target {
            Nv12Target::Buffer {
                y_pitch, uv_pitch, ..
            } => (
                self.buffer_layout(frame.size, output.target)?.2,
                [y_pitch, uv_pitch],
            ),
            _ => ([0, 0], [0, 0]),
        };
        let mut values = [
            frame.origin.0,
            frame.origin.1,
            frame.size.width,
            frame.size.height,
            w,
            h,
            offsets[0],
            offsets[1],
            pitches[0],
            pitches[1],
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        let conversion = output.colour.to_rgb();
        for (i, row) in inverse(conversion.rows).iter().enumerate() {
            for (j, value) in row.iter().enumerate() {
                values[12 + i * 4 + j] = value.to_bits();
            }
            values[15 + i * 4] = conversion.offset[i].to_bits();
        }
        self.queue
            .write_buffer(&self.nv_params, 0, words(&values).as_flattened());
        match output.target {
            Nv12Target::Buffer { buffer, .. } => {
                let (offset, length, _) = self.buffer_layout(frame.size, output.target)?;
                let group = bind(
                    &self.device,
                    &self.nv_buffer,
                    &[
                        wgpu::BindingResource::TextureView(view),
                        self.nv_params.as_entire_binding(),
                        wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer,
                            offset,
                            size: std::num::NonZeroU64::new(length),
                        }),
                    ],
                );
                dispatch(
                    encoder,
                    &self.nv_buffer,
                    &group,
                    w.div_ceil(32),
                    h.div_ceil(8),
                );
            }
            Nv12Target::Textures { y, uv } => {
                let pipeline = self
                    .nv_textures
                    .as_ref()
                    .ok_or(Unsupported("R8/RG8 storage writes"))?;
                let y = y.create_view(&wgpu::TextureViewDescriptor {
                    mip_level_count: Some(1),
                    ..Default::default()
                });
                let uv = uv.create_view(&wgpu::TextureViewDescriptor {
                    mip_level_count: Some(1),
                    ..Default::default()
                });
                let group = bind(
                    &self.device,
                    pipeline,
                    &[
                        wgpu::BindingResource::TextureView(view),
                        self.nv_params.as_entire_binding(),
                        wgpu::BindingResource::TextureView(&y),
                        wgpu::BindingResource::TextureView(&uv),
                    ],
                );
                dispatch(encoder, pipeline, &group, w.div_ceil(8), h.div_ceil(8));
            }
        }
        Ok(())
    }
}
fn inverse(m: [[f32; 3]; 3]) -> [[f32; 3]; 3] {
    let [a, b, c] = m;
    let cross = |x: [f32; 3], y: [f32; 3]| {
        [
            x[1] * y[2] - x[2] * y[1],
            x[2] * y[0] - x[0] * y[2],
            x[0] * y[1] - x[1] * y[0],
        ]
    };
    let cols = [cross(b, c), cross(c, a), cross(a, b)];
    let det = a[0] * cols[0][0] + a[1] * cols[0][1] + a[2] * cols[0][2];
    std::array::from_fn(|i| std::array::from_fn(|j| cols[j][i] / det))
}
fn bind<const N: usize>(
    device: &wgpu::Device,
    pipeline: &wgpu::ComputePipeline,
    resources: &[wgpu::BindingResource<'_>; N],
) -> wgpu::BindGroup {
    let entries: [wgpu::BindGroupEntry<'_>; N] = std::array::from_fn(|i| wgpu::BindGroupEntry {
        binding: i as u32,
        resource: resources[i].clone(),
    });
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("source GPU"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &entries,
    })
}
fn dispatch(
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &wgpu::ComputePipeline,
    group: &wgpu::BindGroup,
    x: u32,
    y: u32,
) {
    let mut pass = encoder.begin_compute_pass(&Default::default());
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, group, &[]);
    pass.dispatch_workgroups(x, y, 1);
}
fn map(
    device: &wgpu::Device,
    buffer: &wgpu::Buffer,
    bytes: u64,
    submission: wgpu::SubmissionIndex,
) -> Result<wgpu::BufferView, SourceGpuError> {
    let start = Instant::now();
    let timeout = Duration::from_secs(2);
    let (send, recv) = mpsc::channel();
    buffer
        .slice(..bytes)
        .map_async(wgpu::MapMode::Read, move |r| {
            let _ = send.send(r);
        });
    let result = (|| {
        device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(timeout),
            })
            .map_err(|e| Gpu(e.to_string()))?;
        recv.recv_timeout(timeout.saturating_sub(start.elapsed()))
            .map_err(|e| Gpu(e.to_string()))?
            .map_err(|e| Gpu(e.to_string()))?;
        buffer
            .slice(..bytes)
            .get_mapped_range()
            .map_err(|e| Gpu(e.to_string()))
    })();
    if result.is_err() {
        buffer.unmap();
    }
    result
}

// wgpu 30 can panic before its error scope sees a handle from another Instance.
// Resources and scopes unwind normally; no panic crosses the source API boundary.
fn catch_gpu<T>(work: impl FnOnce() -> Result<T, SourceGpuError>) -> Result<T, SourceGpuError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("wgpu resource failure");
        Err(Gpu(message.to_owned()))
    })
}
