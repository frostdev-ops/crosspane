use std::{sync::mpsc, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use crosspane_testapp::{
    gpu::PatternRenderer,
    pattern::{ImageSize, rgb_pattern},
};

#[test]
fn gpu_pattern_matches_cpu() -> Result<()> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        compatible_surface: None,
        force_fallback_adapter: false,
        power_preference: wgpu::PowerPreference::None,
        apply_limit_buckets: false,
    })) {
        Ok(adapter) => adapter,
        Err(error) => {
            if std::env::var("CROSSPANE_REQUIRE_GPU").as_deref() == Ok("1") {
                bail!("CROSSPANE_REQUIRE_GPU=1 but no GPU adapter was found: {error}");
            }
            eprintln!("SKIP gpu_pattern_matches_cpu: no GPU adapter found: {error}");
            return Ok(());
        }
    };
    eprintln!("GPU pattern adapter: {:?}", adapter.get_info());
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("offscreen fixture test"),
        ..Default::default()
    }))
    .context("requesting the offscreen GPU device")?;
    let renderer = PatternRenderer::new(&device, wgpu::TextureFormat::Rgba8Unorm);

    for (width, height) in [(300, 200), (1920, 1080)] {
        let size = ImageSize::new(width, height)?;
        let rgb = render_and_read_rgb(&device, &queue, &renderer, size)?;
        let reference = rgb_pattern(size)?;
        ensure!(
            rgb.len() == reference.len(),
            "incorrect RGB readback length"
        );
        if let Some(index) = rgb.iter().zip(&reference).position(|(a, b)| a != b) {
            let pixel = index / 3;
            bail!(
                "{width}x{height}: mismatch at ({}, {}), channel {}: GPU {}, CPU {}",
                pixel % width as usize,
                pixel / width as usize,
                index % 3,
                rgb[index],
                reference[index]
            );
        }
        eprintln!(
            "{width}x{height}: all {} RGB bytes match; alpha is FF",
            rgb.len()
        );
    }
    Ok(())
}

fn render_and_read_rgb(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    renderer: &PatternRenderer,
    size: ImageSize,
) -> Result<Vec<u8>> {
    let extent = wgpu::Extent3d {
        width: size.width(),
        height: size.height(),
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen RGBA8 pattern"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let row_bytes = size.width() * 4;
    let alignment = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded_row_bytes = row_bytes.div_ceil(alignment) * alignment;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("padded pattern readback"),
        size: u64::from(padded_row_bytes) * u64::from(size.height()),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("offscreen pattern and readback"),
    });
    renderer.encode(
        queue,
        &mut encoder,
        &texture.create_view(&Default::default()),
        size,
    );
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row_bytes),
                rows_per_image: Some(size.height()),
            },
        },
        extent,
    );
    let submission = queue.submit([encoder.finish()]);
    let slice = readback.slice(..);
    let (sender, receiver) = mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = sender.send(result);
    });
    device.poll(wgpu::PollType::Wait {
        submission_index: Some(submission),
        timeout: Some(Duration::from_secs(30)),
    })?;
    receiver
        .recv_timeout(Duration::from_secs(30))
        .context("waiting for GPU readback mapping")??;

    let mapped = slice
        .get_mapped_range()
        .context("accessing GPU readback bytes")?;
    let mut rgb = Vec::with_capacity(size.byte_len(3)?);
    for row in mapped.chunks_exact(padded_row_bytes as usize) {
        for pixel in row[..row_bytes as usize].as_chunks::<4>().0 {
            ensure!(pixel[3] == 255, "GPU alpha is not FF");
            rgb.extend_from_slice(&pixel[..3]);
        }
    }
    drop(mapped);
    readback.unmap();
    Ok(rgb)
}
