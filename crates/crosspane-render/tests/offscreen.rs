use std::{
    any::Any,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use crosspane_media::{
    codec::CodecError,
    picture::{NativePicture, Nv12, YuvColour, YuvMatrix, nv12_to_bgra},
};
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
    readback_importing(device, queue, presenter, format, size, None)
}

fn readback_importing(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    presenter: &mut Presenter,
    format: wgpu::TextureFormat,
    size: PixelSize,
    import: Option<gpu::Import<'_>>,
) -> Result<Vec<u8>> {
    presenter.prepare_video(device, queue, import);
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
                            .set_video(&device, visible, full(visible), picture.clone())
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
            .set_video(&device, size, full(size), picture.clone())
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
        .set_video(&device, size, full(size), first)
        .map_err(anyhow::Error::msg)?;
    let second = Arc::new(random_picture(size, 7, YuvColour::default(), 43));
    presenter
        .set_video(&device, size, full(size), second.clone())
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
        .set_video(&device, size, full(size), pending)
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
                .set_video(&device, visible, full(visible), second.clone())
                .is_err()
        );
    }
    let mut invalid = (*second).clone();
    invalid.y.clear();
    assert!(
        presenter
            .set_video(&device, size, full(size), Arc::new(invalid))
            .is_err()
    );
    compare(
        &readback(&device, &queue, &mut presenter, format, size)?,
        &canvas,
        "invalid video leaves canvas unchanged",
    )?;
    Ok(())
}

/// A picture "in native memory" for the tests: its CPU copy is the reference.
#[derive(Debug)]
struct FakeNative(Nv12);

impl NativePicture for FakeNative {
    fn size(&self) -> PixelSize {
        self.0.size
    }
    fn colour(&self) -> YuvColour {
        self.0.colour
    }
    fn to_nv12(&self, out: &mut Nv12) -> Result<(), CodecError> {
        out.clone_from(&self.0);
        Ok(())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Uploads the fake picture's planes into new textures, as a platform importer would wrap them.
fn fake_import(
    queue: &wgpu::Queue,
    device: &wgpu::Device,
    picture: &dyn NativePicture,
    half_height_chroma: u32,
) -> Result<[wgpu::Texture; 2], String> {
    let picture = &picture
        .as_any()
        .downcast_ref::<FakeNative>()
        .ok_or("not a fake picture")?
        .0;
    let plane = |size: PixelSize, format, bytes: &[u8], stride: u32| {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("imported plane"),
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
        });
        queue.write_texture(
            texture.as_image_copy(),
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: None,
            },
            texture.size(),
        );
        texture
    };
    let size = picture.size;
    Ok([
        plane(
            size,
            wgpu::TextureFormat::R8Unorm,
            &picture.y,
            picture.y_stride,
        ),
        plane(
            PixelSize::new(size.width / 2, half_height_chroma),
            wgpu::TextureFormat::Rg8Unorm,
            &picture.uv,
            picture.uv_stride,
        ),
    ])
}

#[test]
fn native_pictures_import_or_copy() -> Result<()> {
    let Some((device, queue)) = device()? else {
        return Ok(());
    };
    let coded = PixelSize::new(64, 48);
    let visible = PixelSize::new(61, 45);
    let target = PixelSize::new(69, 53);
    let imports = Arc::new(AtomicUsize::new(0));
    let counted = {
        let (imports, queue) = (imports.clone(), queue.clone());
        move |device: &wgpu::Device, picture: &dyn NativePicture| {
            imports.fetch_add(1, Ordering::Relaxed);
            fake_import(&queue, device, picture, picture.size().height / 2)
        }
    };
    let failing = |_: &wgpu::Device, _: &dyn NativePicture| -> Result<[wgpu::Texture; 2], String> {
        Err("no import here".into())
    };
    // Planes of the wrong size must not be shown.
    let wrong = {
        let queue = queue.clone();
        move |device: &wgpu::Device, picture: &dyn NativePicture| {
            fake_import(&queue, device, picture, picture.size().height / 2 - 2)
        }
    };
    for format in [
        wgpu::TextureFormat::Bgra8Unorm,
        wgpu::TextureFormat::Bgra8UnormSrgb,
    ] {
        let mut presenter = Presenter::new(&device, format);
        presenter.set_edge([211, 45, 137], 2);
        let importers: [(Option<gpu::Import<'_>>, bool); 4] = [
            (Some(&counted), true),
            (Some(&failing), false),
            (None, false),
            (Some(&wrong), false),
        ];
        for (seed, (import, imported)) in (7..).zip(importers) {
            let colour = YuvColour {
                matrix: YuvMatrix::Bt601,
                full_range: seed % 2 == 0,
            };
            let picture = random_picture(coded, 3, colour, seed);
            let (imports_before, uploads_before) =
                (imports.load(Ordering::Relaxed), presenter.video_uploads);
            presenter
                .set_native_video(
                    &device,
                    visible,
                    full(visible),
                    Arc::new(FakeNative(picture.clone())),
                )
                .map_err(anyhow::Error::msg)?;
            let actual =
                readback_importing(&device, &queue, &mut presenter, format, target, import)?;
            compare_video(&actual, &picture, visible, target, 2)?;
            ensure!(
                presenter.video_uploads == uploads_before + usize::from(!imported),
                "{format:?} seed {seed}: copied {} times",
                presenter.video_uploads - uploads_before
            );
            if imported {
                ensure!(imports.load(Ordering::Relaxed) == imports_before + 1);
            }
            // A CPU picture after an imported one gets its own upload textures again.
            let cpu = Arc::new(random_picture(coded, 0, colour, seed + 100));
            presenter
                .set_video(&device, visible, full(visible), cpu.clone())
                .map_err(anyhow::Error::msg)?;
            let actual = readback(&device, &queue, &mut presenter, format, target)?;
            compare_video(&actual, &cpu, visible, target, 2)?;
        }
        // Odd coded sizes and pictures smaller than the shown size are refused on arrival.
        let odd = Nv12 {
            size: PixelSize::new(63, 48),
            ..random_picture(coded, 0, YuvColour::default(), 3)
        };
        ensure!(
            presenter
                .set_native_video(&device, visible, full(visible), Arc::new(FakeNative(odd)))
                .is_err()
        );
        ensure!(
            presenter
                .set_native_video(
                    &device,
                    PixelSize::new(65, 45),
                    full(PixelSize::new(65, 45)),
                    Arc::new(FakeNative(random_picture(
                        coded,
                        0,
                        YuvColour::default(),
                        4
                    )))
                )
                .is_err()
        );
    }
    Ok(())
}

fn compare_region(
    actual: &[u8],
    canvas: &[u8],
    size: PixelSize,
    picture: &Nv12,
    rect: PixelRect,
    canvas_tiles: &[PixelRect],
) -> Result<()> {
    let visible = PixelSize::new(rect.width() as u32, rect.height() as u32);
    let mut video = Vec::new();
    nv12_to_bgra(picture, visible, &mut video)?;
    for y in 0..size.height {
        for x in 0..size.width {
            let at = (y * size.width + x) as usize * 4;
            let pixel = point2(x as i32, y as i32);
            let canvas_wins = canvas_tiles.iter().any(|dirty| {
                x / 64 >= dirty.min.x as u32 / 64
                    && x / 64 < (dirty.max.x as u32).div_ceil(64)
                    && y / 64 >= dirty.min.y as u32 / 64
                    && y / 64 < (dirty.max.y as u32).div_ceil(64)
            });
            if rect.contains(pixel) && !canvas_wins {
                let source =
                    ((y - rect.min.y as u32) * visible.width + x - rect.min.x as u32) as usize * 4;
                for (got, want) in actual[at..at + 4].iter().zip(&video[source..source + 4]) {
                    ensure!(
                        got.abs_diff(*want) <= 1,
                        "region video ({x},{y}): {got} vs {want}"
                    );
                }
            } else {
                ensure!(
                    actual[at..at + 4] == canvas[at..at + 4],
                    "region canvas ({x},{y})"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn video_regions_and_canvas_tiles() -> Result<()> {
    exercise_video_regions(false)
}

#[test]
fn native_video_region_over_canvas() -> Result<()> {
    exercise_video_regions(true)
}

fn exercise_video_regions(native: bool) -> Result<()> {
    let Some((device, queue)) = device()? else {
        return Ok(());
    };
    let size = PixelSize::new(269, 211);
    let importer = |device: &wgpu::Device, picture: &dyn NativePicture| {
        fake_import(&queue, device, picture, picture.size().height / 2)
    };
    let import: Option<gpu::Import<'_>> = if native { Some(&importer) } else { None };
    let set_picture = |presenter: &mut Presenter, rect: PixelRect, picture: Arc<Nv12>| {
        if native {
            presenter.set_native_video(
                &device,
                size,
                rect,
                Arc::new(FakeNative((*picture).clone())),
            )
        } else {
            presenter.set_video(&device, size, rect, picture)
        }
    };
    // Interior and right/bottom partial tiles; coded padding must never appear.
    for rect in [
        PixelRect::new(point2(64, 64), point2(192, 192)),
        PixelRect::new(point2(128, 64), point2(269, 211)),
    ] {
        for format in [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Bgra8UnormSrgb,
        ] {
            let mut presenter = Presenter::new(&device, format);
            let mut canvas = pattern(size, 0);
            presenter
                .upload(&device, &queue, size, &canvas, &[full(size)])
                .map_err(anyhow::Error::msg)?;
            let picture = Arc::new(random_picture(
                PixelSize::new(144, 148),
                7,
                YuvColour::default(),
                53,
            ));
            set_picture(&mut presenter, rect, picture.clone()).map_err(anyhow::Error::msg)?;
            compare_region(
                &readback_importing(&device, &queue, &mut presenter, format, size, import)?,
                &canvas,
                size,
                &picture,
                rect,
                &[],
            )?;
            // A tiny dirty rectangle switches its entire tile to canvas, preserving the
            // previously uploaded canvas outside the dirty pixels in that tile.
            let dirty = PixelRect::new(
                rect.min + point2(3, 4).to_vector(),
                rect.min + point2(9, 11).to_vector(),
            );
            let new_canvas = pattern(size, 1);
            for y in dirty.min.y..dirty.max.y {
                let start = (y as u32 * size.width + dirty.min.x as u32) as usize * 4;
                let end = start + dirty.width() as usize * 4;
                canvas[start..end].copy_from_slice(&new_canvas[start..end]);
            }
            presenter
                .upload(&device, &queue, size, &new_canvas, &[dirty])
                .map_err(anyhow::Error::msg)?;
            compare_region(
                &readback_importing(&device, &queue, &mut presenter, format, size, import)?,
                &canvas,
                size,
                &picture,
                rect,
                &[dirty],
            )?;
            let smaller = PixelRect::new(point2(192, 128), point2(256, 192));
            let second = Arc::new(random_picture(
                PixelSize::new(66, 68),
                3,
                YuvColour::default(),
                54,
            ));
            set_picture(&mut presenter, smaller, second.clone()).map_err(anyhow::Error::msg)?;
            compare_region(
                &readback_importing(&device, &queue, &mut presenter, format, size, import)?,
                &canvas,
                size,
                &second,
                smaller,
                &[],
            )?;
            // Cancelling a moved pending region must not revive the previous picture
            // at its old position, even though its tiles still say video.
            set_picture(&mut presenter, rect, picture.clone()).map_err(anyhow::Error::msg)?;
            presenter
                .upload(&device, &queue, size, &canvas, &[rect])
                .map_err(anyhow::Error::msg)?;
            compare(
                &readback_importing(&device, &queue, &mut presenter, format, size, import)?,
                &canvas,
                "cancelled region cannot revive old video",
            )?;
            // Arrival ordering also applies before the newest picture has been uploaded.
            set_picture(&mut presenter, rect, picture.clone()).map_err(anyhow::Error::msg)?;
            presenter
                .upload(&device, &queue, size, &canvas, &[dirty])
                .map_err(anyhow::Error::msg)?;
            compare_region(
                &readback_importing(&device, &queue, &mut presenter, format, size, import)?,
                &canvas,
                size,
                &picture,
                rect,
                &[dirty],
            )?;
            if native {
                ensure!(
                    presenter.video_uploads == 0,
                    "region importer copied a picture"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn source_map_size_changes_and_invalid_regions() -> Result<()> {
    let Some((device, queue)) = device()? else {
        return Ok(());
    };
    for format in [
        wgpu::TextureFormat::Bgra8Unorm,
        wgpu::TextureFormat::Bgra8UnormSrgb,
    ] {
        let mut presenter = Presenter::new(&device, format);
        let old = PixelSize::new(192, 192);
        let size = PixelSize::new(141, 133);
        let picture = Arc::new(random_picture(
            PixelSize::new(80, 70),
            0,
            YuvColour::default(),
            56,
        ));
        presenter
            .upload(&device, &queue, old, &pattern(old, 0), &[full(old)])
            .map_err(anyhow::Error::msg)?;
        let rect = PixelRect::new(point2(64, 64), point2(141, 133));
        presenter
            .set_video(&device, size, rect, picture.clone())
            .map_err(anyhow::Error::msg)?;
        let grey: Vec<u8> = [32, 32, 32, 255].repeat((size.width * size.height) as usize);
        compare_region(
            &readback(&device, &queue, &mut presenter, format, size)?,
            &grey,
            size,
            &picture,
            rect,
            &[],
        )?;
        for invalid in [
            PixelRect::new(point2(64, 64), point2(64, 128)),
            PixelRect::new(point2(-64, 0), point2(64, 64)),
            PixelRect::new(point2(1, 64), point2(64, 128)),
            PixelRect::new(point2(64, 64), point2(140, 128)),
            PixelRect::new(point2(64, 64), point2(192, 128)),
            full(size),
        ] {
            ensure!(
                presenter
                    .set_video(&device, size, invalid, picture.clone())
                    .is_err()
            );
            ensure!(
                presenter
                    .set_native_video(
                        &device,
                        size,
                        invalid,
                        Arc::new(FakeNative((*picture).clone()))
                    )
                    .is_err()
            );
        }
        // Invalid arrivals cannot mutate either layer or the map.
        compare_region(
            &readback(&device, &queue, &mut presenter, format, size)?,
            &grey,
            size,
            &picture,
            rect,
            &[],
        )?;
        let canvas = pattern(old, 1);
        presenter
            .upload(&device, &queue, old, &canvas, &[full(old)])
            .map_err(anyhow::Error::msg)?;
        compare(
            &readback(&device, &queue, &mut presenter, format, old)?,
            &canvas,
            "Frame size change clears video",
        )?;
        // Same-size video after the reset proves old canvas selections were reset too.
        let rect = PixelRect::new(point2(64, 64), point2(128, 128));
        presenter
            .set_video(&device, old, rect, picture.clone())
            .map_err(anyhow::Error::msg)?;
        compare_region(
            &readback(&device, &queue, &mut presenter, format, old)?,
            &canvas,
            old,
            &picture,
            rect,
            &[],
        )?;
    }
    Ok(())
}
