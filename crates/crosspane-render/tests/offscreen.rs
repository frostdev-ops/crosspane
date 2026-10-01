use std::{sync::mpsc, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use crosspane_types::geom::{PixelRect, PixelSize, euclid::point2};

// Compile the private implementation, without adding a public testing API to the frozen host.
#[path = "../src/proxy/gpu.rs"]
#[allow(dead_code)]
mod gpu;
use gpu::Presenter;

fn device() -> Result<Option<(wgpu::Device, wgpu::Queue)>> {
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
            eprintln!("SKIP offscreen: no GPU adapter found: {error}");
            return Ok(None);
        }
    };
    eprintln!("offscreen adapter: {:?}", adapter.get_info());
    Ok(Some(pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("proxy byte-exact tests"),
            ..Default::default()
        },
    ))?))
}

fn full(size: PixelSize) -> PixelRect {
    PixelRect::new(point2(0, 0), point2(size.width as i32, size.height as i32))
}

fn pattern(size: PixelSize, kind: u32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(size.width as usize * size.height as usize * 4);
    let mut random = 0xC05F_A123_u32;
    for y in 0..size.height {
        for x in 0..size.width {
            for channel in 0..4 {
                random ^= random << 13;
                random ^= random >> 17;
                random ^= random << 5;
                bytes.push(match kind {
                    0 => random as u8,
                    1 => ((x + 3 * y + channel * 53) % 256) as u8,
                    _ => {
                        if (x + y + channel) % 2 == 0 {
                            0
                        } else {
                            255
                        }
                    }
                });
            }
        }
    }
    bytes
}

#[test]
fn bit_exact_noise_gradients_checkerboard() -> Result<()> {
    let Some((device, queue)) = device()? else {
        return Ok(());
    };
    for format in [
        wgpu::TextureFormat::Bgra8Unorm,
        wgpu::TextureFormat::Bgra8UnormSrgb,
    ] {
        let mut presenter = Presenter::new(&device, format);
        for (width, height) in [(1, 1), (63, 65), (1920, 1080)] {
            let size = PixelSize::new(width, height);
            for kind in 0..3 {
                let bytes = pattern(size, kind);
                presenter
                    .upload(&device, &queue, size, &bytes, &[full(size)])
                    .map_err(anyhow::Error::msg)?;
                let actual = readback(&device, &queue, &presenter, format, size)?;
                compare(
                    &actual,
                    &bytes,
                    &format!("{format:?} {width}x{height} pattern {kind}"),
                )?;
                eprintln!(
                    "{format:?} {width}x{height} pattern {kind}: {} bytes exact",
                    bytes.len()
                );
            }
            // A larger window must preserve the top-left canvas and fill exposed area #202020.
            let target_size = PixelSize::new(width + 3, height + 2);
            let canvas = pattern(size, 2);
            let actual = readback(&device, &queue, &presenter, format, target_size)?;
            for y in 0..target_size.height {
                for x in 0..target_size.width {
                    let offset = (y as usize * target_size.width as usize + x as usize) * 4;
                    let expected = if x < width && y < height {
                        let source = (y as usize * width as usize + x as usize) * 4;
                        &canvas[source..source + 4]
                    } else {
                        &[32, 32, 32, 255]
                    };
                    ensure!(
                        &actual[offset..offset + 4] == expected,
                        "{format:?}: canvas/grey mismatch ({x},{y})"
                    );
                }
            }
            // A smaller target crops without resampling.
            let cropped = PixelSize::new(
                width.saturating_sub(2).max(1),
                height.saturating_sub(2).max(1),
            );
            let actual = readback(&device, &queue, &presenter, format, cropped)?;
            let expected: Vec<_> = canvas
                .chunks_exact(width as usize * 4)
                .take(cropped.height as usize)
                .flat_map(|row| row[..cropped.width as usize * 4].iter().copied())
                .collect();
            compare(&actual, &expected, "top-left crop")?;
        }
    }
    Ok(())
}

#[test]
fn dirty_rect_partial_updates_are_exact() -> Result<()> {
    let Some((device, queue)) = device()? else {
        return Ok(());
    };
    for format in [
        wgpu::TextureFormat::Bgra8Unorm,
        wgpu::TextureFormat::Bgra8UnormSrgb,
    ] {
        let mut presenter = Presenter::new(&device, format);
        for (width, height) in [(1, 1), (63, 65), (1920, 1080)] {
            let size = PixelSize::new(width, height);
            let mut expected = pattern(size, 0);
            presenter
                .upload(&device, &queue, size, &expected, &[full(size)])
                .map_err(anyhow::Error::msg)?;
            let updated = pattern(size, 1);
            let dirty = [
                PixelRect::new(
                    point2(0, 0),
                    point2(width.div_ceil(2) as i32, height.div_ceil(2) as i32),
                ),
                PixelRect::new(
                    point2((width / 3) as i32, (height / 3) as i32),
                    point2(width as i32, height as i32),
                ),
            ];
            presenter
                .upload(&device, &queue, size, &updated, &dirty)
                .map_err(anyhow::Error::msg)?;
            for rect in dirty {
                for y in rect.min.y..rect.max.y {
                    let start = (y as usize * width as usize + rect.min.x as usize) * 4;
                    let end = start + (rect.max.x - rect.min.x) as usize * 4;
                    expected[start..end].copy_from_slice(&updated[start..end]);
                }
            }
            compare(
                &readback(&device, &queue, &presenter, format, size)?,
                &expected,
                "dirty update",
            )?;
            presenter
                .upload(&device, &queue, size, &updated, &[])
                .map_err(anyhow::Error::msg)?;
            let invalid = PixelRect::new(point2(-1, 0), point2(1, 1));
            assert!(
                presenter
                    .upload(&device, &queue, size, &updated, &[full(size), invalid])
                    .is_err()
            );
            assert!(presenter.upload(&device, &queue, size, &[], &[]).is_err());
            compare(
                &readback(&device, &queue, &presenter, format, size)?,
                &expected,
                "empty/invalid damage is unchanged",
            )?;
            eprintln!("{format:?} {width}x{height}: dirty rectangles exact");
        }
    }
    Ok(())
}

fn compare(actual: &[u8], expected: &[u8], description: &str) -> Result<()> {
    ensure!(
        actual.len() == expected.len(),
        "{description}: wrong byte count"
    );
    if let Some(index) = actual.iter().zip(expected).position(|(a, b)| a != b) {
        bail!(
            "{description}: byte {index}: GPU {}, canvas {}",
            actual[index],
            expected[index]
        );
    }
    Ok(())
}

fn readback(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    presenter: &Presenter,
    format: wgpu::TextureFormat,
    size: PixelSize,
) -> Result<Vec<u8>> {
    let extent = wgpu::Extent3d {
        width: size.width,
        height: size.height,
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("proxy offscreen surface"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let padded_row = (size.width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("proxy readback"),
        size: u64::from(padded_row) * u64::from(size.height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("proxy offscreen presentation/readback"),
    });
    presenter.encode(&mut encoder, &texture.create_view(&Default::default()));
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: Some(size.height),
            },
        },
        extent,
    );
    let submission = queue.submit([encoder.finish()]);
    let (sender, receiver) = mpsc::channel();
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
    device.poll(wgpu::PollType::Wait {
        submission_index: Some(submission),
        timeout: Some(Duration::from_secs(30)),
    })?;
    receiver
        .recv_timeout(Duration::from_secs(30))
        .context("waiting for readback")??;
    let mapped = buffer.slice(..).get_mapped_range()?;
    let bytes = mapped
        .chunks_exact(padded_row as usize)
        .flat_map(|row| row[..size.width as usize * 4].iter().copied())
        .collect();
    drop(mapped);
    buffer.unmap();
    Ok(bytes)
}
