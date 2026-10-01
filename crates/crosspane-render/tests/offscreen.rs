use std::{
    sync::{Arc, mpsc},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use crosspane_media::picture::{Nv12, YuvColour, YuvMatrix, nv12_to_bgra};
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
                let actual = readback(&device, &queue, &mut presenter, format, size)?;
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
            let actual = readback(&device, &queue, &mut presenter, format, target_size)?;
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
            let actual = readback(&device, &queue, &mut presenter, format, cropped)?;
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
                    point2((width / 2) as i32, 0),
                    point2(width as i32, height.div_ceil(3) as i32),
                ),
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
                &readback(&device, &queue, &mut presenter, format, size)?,
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
                &readback(&device, &queue, &mut presenter, format, size)?,
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
    presenter: &mut Presenter,
    format: wgpu::TextureFormat,
    size: PixelSize,
) -> Result<Vec<u8>> {
    presenter.prepare_video(device, queue);
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
    presenter.encode(
        queue,
        size,
        &mut encoder,
        &texture.create_view(&Default::default()),
    );
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

#[test]
fn edge_preserves_pattern_and_grey() -> Result<()> {
    let Some((device, queue)) = device()? else {
        return Ok(());
    };
    let canvas_size = PixelSize::new(37, 31);
    let size = PixelSize::new(43, 39);
    let bytes = pattern(canvas_size, 0);
    let accent = [211, 45, 137];
    for format in [
        wgpu::TextureFormat::Bgra8Unorm,
        wgpu::TextureFormat::Bgra8UnormSrgb,
    ] {
        let mut presenter = Presenter::new(&device, format);
        presenter
            .upload(&device, &queue, canvas_size, &bytes, &[full(canvas_size)])
            .map_err(anyhow::Error::msg)?;
        for edge in [2, 8] {
            presenter.set_edge(accent, edge);
            let actual = readback(&device, &queue, &mut presenter, format, size)?;
            for y in 0..size.height {
                for x in 0..size.width {
                    let offset = (y * size.width + x) as usize * 4;
                    let pixel = &actual[offset..offset + 4];
                    if x < edge || y < edge || size.width - x <= edge || size.height - y <= edge {
                        for (actual, expected) in
                            pixel.iter().zip([accent[2], accent[1], accent[0], 255])
                        {
                            ensure!(
                                actual.abs_diff(expected) <= 1,
                                "{format:?} edge {edge}: accent mismatch ({x},{y})"
                            );
                        }
                    } else if x < canvas_size.width && y < canvas_size.height {
                        let source = (y * canvas_size.width + x) as usize * 4;
                        ensure!(
                            pixel == &bytes[source..source + 4],
                            "{format:?} edge {edge}: interior mismatch ({x},{y})"
                        );
                    } else {
                        ensure!(
                            pixel == [32, 32, 32, 255],
                            "{format:?} edge {edge}: grey mismatch ({x},{y})"
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

fn random_picture(size: PixelSize, padding: u32, colour: YuvColour, seed: u32) -> Nv12 {
    let stride = size.width + padding;
    // Exercise the shortest valid final row as well as padded rows.
    let mut picture = Nv12 {
        size,
        y: vec![0; ((size.height - 1) * stride + size.width) as usize],
        uv: vec![0; ((size.height / 2 - 1) * stride + size.width) as usize],
        y_stride: stride,
        uv_stride: stride,
        colour,
    };
    let mut random = seed;
    for sample in picture.y.iter_mut().chain(&mut picture.uv) {
        random ^= random << 13;
        random ^= random >> 17;
        random ^= random << 5;
        *sample = random as u8;
    }
    picture
}

fn compare_video(
    actual: &[u8],
    picture: &Nv12,
    visible: PixelSize,
    target: PixelSize,
    edge: u32,
) -> Result<()> {
    let mut expected = Vec::new();
    nv12_to_bgra(picture, visible, &mut expected)?;
    for y in 0..target.height {
        for x in 0..target.width {
            let at = (y * target.width + x) as usize * 4;
            if x < edge || y < edge || target.width - x <= edge || target.height - y <= edge {
                for (got, want) in actual[at..at + 4].iter().zip([137, 45, 211, 255]) {
                    ensure!(got.abs_diff(want) <= 1, "video accent at ({x},{y})");
                }
            } else if x < visible.width && y < visible.height {
                let source = (y * visible.width + x) as usize * 4;
                for (got, want) in actual[at..at + 4].iter().zip(&expected[source..source + 4]) {
                    ensure!(
                        got.abs_diff(*want) <= 1,
                        "NV12 reference at ({x},{y}): {got} vs {want}"
                    );
                }
            } else {
                ensure!(
                    actual[at..at + 4] == [32, 32, 32, 255],
                    "coded padding shown at ({x},{y})"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn nv12_matches_reference() -> Result<()> {
    let Some((device, queue)) = device()? else {
        return Ok(());
    };
    for format in [
        wgpu::TextureFormat::Bgra8Unorm,
        wgpu::TextureFormat::Bgra8UnormSrgb,
    ] {
        let mut presenter = Presenter::new(&device, format);
        for matrix in [YuvMatrix::Bt601, YuvMatrix::Bt709] {
            for full_range in [false, true] {
                for padding in [0, 7] {
                    let coded = PixelSize::new(64, 48);
                    for visible in [coded, PixelSize::new(61, 45)] {
                        let picture = Arc::new(random_picture(
                            coded,
                            padding,
                            YuvColour { matrix, full_range },
                            0xC05F_A123,
                        ));
                        presenter
                            .set_video(&device, visible, picture.clone())
                            .map_err(anyhow::Error::msg)?;
                        presenter.set_edge([211, 45, 137], 2);
                        let target = PixelSize::new(69, 53);
                        let actual = readback(&device, &queue, &mut presenter, format, target)?;
                        compare_video(&actual, &picture, visible, target, 2)?;
                        // Same picture persists over a redraw; a smaller window crops at top-left.
                        let target = PixelSize::new(39, 33);
                        let actual = readback(&device, &queue, &mut presenter, format, target)?;
                        compare_video(&actual, &picture, visible, target, 2)?;
                    }
                }
            }
        }
    }
    Ok(())
}

#[test]
fn canvas_video_canvas_switching() -> Result<()> {
    let Some((device, queue)) = device()? else {
        return Ok(());
    };
    let size = PixelSize::new(38, 32);
    for format in [
        wgpu::TextureFormat::Bgra8Unorm,
        wgpu::TextureFormat::Bgra8UnormSrgb,
    ] {
        let mut presenter = Presenter::new(&device, format);
        let first = pattern(size, 0);
        presenter
            .upload(&device, &queue, size, &first, &[full(size)])
            .map_err(anyhow::Error::msg)?;
        compare(
            &readback(&device, &queue, &mut presenter, format, size)?,
            &first,
            "first canvas",
        )?;
        let picture = Arc::new(random_picture(size, 6, YuvColour::default(), 41));
        presenter
            .set_video(&device, size, picture.clone())
            .map_err(anyhow::Error::msg)?;
        compare_video(
            &readback(&device, &queue, &mut presenter, format, size)?,
            &picture,
            size,
            size,
            0,
        )?;
        let last = pattern(size, 1);
        presenter
            .upload(&device, &queue, size, &last, &[full(size)])
            .map_err(anyhow::Error::msg)?;
        compare(
            &readback(&device, &queue, &mut presenter, format, size)?,
            &last,
            "last canvas",
        )?;
    }
    Ok(())
}

#[test]
fn superseded_pictures_never_reach_upload() -> Result<()> {
    let Some((device, queue)) = device()? else {
        return Ok(());
    };
    let size = PixelSize::new(38, 32);
    let format = wgpu::TextureFormat::Bgra8Unorm;
    let mut presenter = Presenter::new(&device, format);
    let first = Arc::new(random_picture(size, 0, YuvColour::default(), 42));
    let dropped = Arc::downgrade(&first);
    presenter
        .set_video(&device, size, first)
        .map_err(anyhow::Error::msg)?;
    let second = Arc::new(random_picture(size, 7, YuvColour::default(), 43));
    presenter
        .set_video(&device, size, second.clone())
        .map_err(anyhow::Error::msg)?;
    ensure!(
        dropped.upgrade().is_none(),
        "superseded pending picture retained"
    );
    ensure!(
        presenter.video_uploads == 0,
        "pending pictures uploaded on arrival"
    );
    // readback calls prepare_video, which consumes only the remaining slot.
    compare_video(
        &readback(&device, &queue, &mut presenter, format, size)?,
        &second,
        size,
        size,
        0,
    )?;
    ensure!(
        presenter.video_uploads == 1,
        "superseded picture was uploaded"
    );
    let pending = Arc::new(random_picture(size, 0, YuvColour::default(), 44));
    let dropped = Arc::downgrade(&pending);
    presenter
        .set_video(&device, size, pending)
        .map_err(anyhow::Error::msg)?;
    let canvas = pattern(size, 0);
    presenter
        .upload(&device, &queue, size, &canvas, &[full(size)])
        .map_err(anyhow::Error::msg)?;
    ensure!(
        dropped.upgrade().is_none(),
        "canvas did not cancel pending video"
    );
    compare(
        &readback(&device, &queue, &mut presenter, format, size)?,
        &canvas,
        "canvas supersedes video",
    )?;
    ensure!(
        presenter.video_uploads == 1,
        "canvas-cancelled picture was uploaded"
    );
    for visible in [PixelSize::new(0, 1), PixelSize::new(39, 32)] {
        assert!(
            presenter
                .set_video(&device, visible, second.clone())
                .is_err()
        );
    }
    let mut invalid = (*second).clone();
    invalid.y.clear();
    assert!(
        presenter
            .set_video(&device, size, Arc::new(invalid))
            .is_err()
    );
    compare(
        &readback(&device, &queue, &mut presenter, format, size)?,
        &canvas,
        "invalid video leaves canvas unchanged",
    )?;
    Ok(())
}
