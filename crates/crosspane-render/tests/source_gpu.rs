use anyhow::{Result, bail, ensure};
use crosspane_media::{
    picture::{Nv12, YuvColour, YuvMatrix, nv12_to_bgra},
    tiles::TileSource,
};
use crosspane_render::source::*;
use crosspane_types::geom::PixelSize;
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};
fn gpu() -> Result<Option<SourceGpu>> {
    gpu_with_features(true)
}
fn gpu_with_features(optional: bool) -> Result<Option<SourceGpu>> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN | wgpu::Backends::METAL,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        ..Default::default()
    })) {
        Ok(a) => a,
        Err(e) => {
            if std::env::var("CROSSPANE_REQUIRE_GPU").as_deref() == Ok("1") {
                bail!("GPU required: {e}");
            }
            eprintln!("SKIP source GPU: {e}");
            return Ok(None);
        }
    };
    eprintln!("source GPU adapter: {:?}", adapter.get_info());
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        required_features: if optional {
            SourceGpu::optional_features(&adapter)
        } else {
            wgpu::Features::empty()
        },
        ..Default::default()
    }))?;
    let gpu = SourceGpu::new(device, queue)?;
    eprintln!(
        "NV12 storage textures supported: {}",
        gpu.nv12_textures_supported()
    );
    Ok(Some(gpu))
}
fn texture(
    g: &SourceGpu,
    size: PixelSize,
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
) -> wgpu::Texture {
    g.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("source test"),
        size: wgpu::Extent3d {
            width: size.width,
            height: size.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    })
}
fn upload(g: &SourceGpu, size: PixelSize, data: &[u8]) -> wgpu::Texture {
    let t = texture(
        g,
        size,
        wgpu::TextureFormat::Bgra8Unorm,
        wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
    );
    write(g, &t, data);
    t
}
fn write(g: &SourceGpu, t: &wgpu::Texture, data: &[u8]) {
    g.queue().write_texture(
        t.as_image_copy(),
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(t.width() * 4),
            rows_per_image: None,
        },
        t.size(),
    );
}
fn region(t: &wgpu::Texture) -> FrameRegion<'_> {
    FrameRegion {
        texture: t,
        origin: (0, 0),
        size: PixelSize::new(t.width(), t.height()),
    }
}
fn rand(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}
fn random(size: PixelSize, state: &mut u32) -> Vec<u8> {
    (0..size.width * size.height * 4)
        .map(|_| rand(state) as u8)
        .collect()
}
fn packed(bytes: &[u8], size: PixelSize, tx: u32, ty: u32) -> Vec<u8> {
    let w = (size.width - tx * 64).min(64);
    let h = (size.height - ty * 64).min(64);
    (0..h)
        .flat_map(|r| {
            let i = ((ty * 64 + r) * size.width + tx * 64) as usize * 4;
            bytes[i..i + w as usize * 4].iter().copied()
        })
        .collect()
}
fn truth(a: &[u8], b: &[u8], size: PixelSize) -> Vec<u32> {
    let nx = size.width.div_ceil(64);
    let ny = size.height.div_ceil(64);
    let mut bits = vec![0; (nx * ny).div_ceil(32) as usize];
    for i in 0..nx * ny {
        if packed(a, size, i % nx, i / nx) != packed(b, size, i % nx, i / nx) {
            bits[(i / 32) as usize] |= 1 << (i % 32);
        }
    }
    bits
}
#[test]
fn change_sets_equal_truth() -> Result<()> {
    let Some(mut g) = gpu_with_features(false)? else {
        return Ok(());
    };
    let mut seed = 0x12345678;
    let sizes = [(1, 1), (63, 65), (64, 64), (65, 63), (130, 70), (333, 211)];
    let small = upload(&g, PixelSize::new(1, 1), &[7, 19, 43, 255]);
    g.scan(region(&small), None, false)?;
    g.commit();
    g.commit();
    let large = upload(
        &g,
        PixelSize::new(333, 211),
        &random(PixelSize::new(333, 211), &mut seed),
    );
    g.scan(region(&large), None, false)?;
    ensure!(
        g.scan(region(&small), None, false)?.changed == 0,
        "uncommitted growth lost committed hashes"
    );

    for trial in 0..1000 {
        let (w, h) = sizes[trial % sizes.len()];
        let size = PixelSize::new(w, h);
        let previous = random(size, &mut seed);
        let mut next = previous.clone();
        let t = upload(&g, size, &previous);
        g.scan(region(&t), None, false)?;
        g.commit();
        let x = rand(&mut seed) % w;
        let y = rand(&mut seed) % h;
        match trial % 5 {
            0 => next[((y * w + x) * 4) as usize] ^= 1,
            1 => {
                for yy in (y / 64 * 64)..(y / 64 * 64 + 64).min(h) {
                    for xx in (x / 64 * 64)..(x / 64 * 64 + 64).min(w) {
                        next[((yy * w + xx) * 4) as usize] ^= 128;
                    }
                }
            }
            2 => {
                for xx in 0..w {
                    next[((y * w + xx) * 4 + 3) as usize] ^= 1;
                }
            }
            3 => next = random(size, &mut seed),
            _ => {}
        }
        // A superseded scan must never become the reference.
        let scratch = random(size, &mut seed);
        write(&g, &t, &scratch);
        g.scan(region(&t), None, false)?;
        write(&g, &t, &next);
        let result = g.scan(region(&t), None, false)?;
        ensure!(
            result.changed_bits == truth(&previous, &next, size),
            "trial {trial}"
        );
        g.commit();
        ensure!(g.scan(region(&t), None, false)?.changed == 0);
        if trial % 37 == 0 {
            ensure!(g.scan(region(&t), None, true)?.changed == w.div_ceil(64) * h.div_ceil(64));
            g.reset();
            ensure!(g.scan(region(&t), None, false)?.changed == w.div_ceil(64) * h.div_ceil(64));
        }
    }
    // Size changes within the same tile grid must also invalidate the reference.
    let t = upload(
        &g,
        PixelSize::new(63, 63),
        &random(PixelSize::new(63, 63), &mut seed),
    );
    g.scan(region(&t), None, false)?;
    g.commit();
    let t = upload(
        &g,
        PixelSize::new(64, 64),
        &random(PixelSize::new(64, 64), &mut seed),
    );
    ensure!(g.scan(region(&t), None, false)?.changed == 1);
    Ok(())
}
#[test]
fn permutations_are_changes() -> Result<()> {
    let Some(mut g) = gpu()? else { return Ok(()) };
    let size = PixelSize::new(64, 64);
    let mut seed = 41;
    let a = random(size, &mut seed);
    let t = upload(&g, size, &a);
    g.scan(region(&t), None, false)?;
    g.commit();
    for rows in [true, false] {
        let mut b = a.clone();
        for i in 0..64 {
            for c in 0..4 {
                let (x, y) = if rows {
                    ((7 * 64 + i) * 4 + c, (23 * 64 + i) * 4 + c)
                } else {
                    ((i * 64 + 7) * 4 + c, (i * 64 + 23) * 4 + c)
                };
                b.swap(x, y);
            }
        }
        write(&g, &t, &b);
        ensure!(g.scan(region(&t), None, false)?.changed == 1);
    }
    Ok(())
}
#[test]
fn regions_and_gather_are_byte_exact() -> Result<()> {
    let Some(mut g) = gpu_with_features(false)? else {
        return Ok(());
    };
    let size = PixelSize::new(130, 70);
    let outer = PixelSize::new(153, 89);
    // Exhaust every byte value in every channel, including alpha.
    let bytes: Vec<u8> = (0..size.width * size.height * 4)
        .map(|i| (i / 4 + i % 4 * 53) as u8)
        .collect();
    let mut image = vec![119; (outer.width * outer.height * 4) as usize];
    for y in 0..size.height {
        let dst = ((y + 9) * outer.width + 11) as usize * 4;
        let src = (y * size.width) as usize * 4;
        image[dst..dst + size.width as usize * 4]
            .copy_from_slice(&bytes[src..src + size.width as usize * 4]);
    }
    let t = upload(&g, outer, &image);
    let frame = FrameRegion {
        texture: &t,
        origin: (11, 9),
        size,
    };
    let initial = g.scan(frame, None, false)?;
    g.commit();
    image[0] ^= 255;
    write(&g, &t, &image);
    ensure!(g.scan(frame, None, false)?.changed == 0);
    let partial = TileChanges {
        changed_bits: vec![0b100101],
        changed: 3,
        ..initial
    };
    for all in [false, true] {
        let gathered = g.gather(frame, &partial, all)?;
        for ty in 0..2 {
            for tx in 0..3 {
                let selected = all || partial.changed_bits[0] & (1 << (ty * 3 + tx)) != 0;
                if selected {
                    ensure!(gathered.tile(tx, ty) == Some(packed(&bytes, size, tx, ty).as_slice()));
                } else {
                    ensure!(gathered.tile(tx, ty).is_none());
                }
            }
        }
        ensure!(gathered.tile(3, 0).is_none());
        ensure!(gathered.tile(0, 2).is_none());
    }
    let empty = TileChanges {
        changed_bits: vec![0],
        changed: 0,
        ..partial.clone()
    };
    ensure!(g.gather(frame, &empty, false)?.tile(0, 0).is_none());
    let bad = TileChanges {
        tiles_x: 2,
        ..partial
    };
    ensure!(g.gather(frame, &bad, false).is_err());
    ensure!(
        g.scan(
            FrameRegion {
                origin: (outer.width, 0),
                ..frame
            },
            None,
            false
        )
        .is_err()
    );
    // A validation failure must not panic or erase the last successful pending scan.
    image[((10 * outer.width + 12) * 4) as usize] ^= 1;
    write(&g, &t, &image);
    ensure!(g.scan(frame, None, false)?.changed == 1);
    let Some(other) = gpu_with_features(false)? else {
        return Ok(());
    };
    let foreign = texture(
        &other,
        PixelSize::new(500, 256),
        wgpu::TextureFormat::Bgra8Unorm,
        wgpu::TextureUsages::TEXTURE_BINDING,
    );
    ensure!(matches!(
        g.scan(region(&foreign), None, false),
        Err(SourceGpuError::Gpu(_))
    ));
    g.commit();
    ensure!(g.scan(frame, None, false)?.changed == 0);

    Ok(())
}
fn read_buffer(g: &SourceGpu, b: &wgpu::Buffer) -> Result<Vec<u8>> {
    let (send, recv) = mpsc::channel();
    b.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = send.send(r);
    });
    g.device().poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: Some(Duration::from_secs(2)),
    })?;
    recv.recv_timeout(Duration::from_secs(2))??;
    let v = b.slice(..).get_mapped_range()?;
    let bytes = v.to_vec();
    drop(v);
    b.unmap();
    Ok(bytes)
}
fn read_texture(g: &SourceGpu, t: &wgpu::Texture, bpp: u32) -> Result<Vec<u8>> {
    let pitch = (t.width() * bpp).next_multiple_of(256);
    let b = g.device().create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(pitch) * u64::from(t.height()),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut e = g.device().create_command_encoder(&Default::default());
    e.copy_texture_to_buffer(
        t.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &b,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(pitch),
                rows_per_image: None,
            },
        },
        t.size(),
    );
    g.queue().submit([e.finish()]);
    Ok(read_buffer(g, &b)?
        .chunks_exact(pitch as usize)
        .flat_map(|r| r[..(t.width() * bpp) as usize].iter().copied())
        .collect())
}
fn reference(bytes: &[u8], size: PixelSize, colour: YuvColour) -> Nv12 {
    // Invert the frozen map independently through its luma/chroma coefficients.
    let m = colour.to_rgb();
    let l = m.rows[0][0];
    let s = if colour.full_range {
        1.0
    } else {
        255.0 / 224.0
    };
    let kr = 1.0 - m.rows[0][2] / (2.0 * s);
    let kb = 1.0 - m.rows[2][1] / (2.0 * s);
    let kg = 1.0 - kr - kb;
    let code = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    let rgb = |x: u32, y: u32| {
        let i = (y.min(size.height - 1) * size.width + x.min(size.width - 1)) as usize * 4;
        [
            bytes[i + 2] as f32 / 255.0,
            bytes[i + 1] as f32 / 255.0,
            bytes[i] as f32 / 255.0,
        ]
    };
    let w = size.width.next_multiple_of(2);
    let h = size.height.next_multiple_of(2);
    let mut y = Vec::new();
    let mut uv = Vec::new();
    for yy in 0..h {
        for x in 0..w {
            let [r, g, b] = rgb(x, yy);
            y.push(code((kr * r + kg * g + kb * b) / l + m.offset[0]));
        }
    }
    for yy in (0..h).step_by(2) {
        for x in (0..w).step_by(2) {
            let mut v = [0.0; 3];
            for dy in 0..2 {
                for dx in 0..2 {
                    let p = rgb(x + dx, yy + dy);
                    for c in 0..3 {
                        v[c] += p[c] * 0.25;
                    }
                }
            }
            let [r, g, b] = v;
            let lum = kr * r + kg * g + kb * b;
            uv.push(code((b - lum) / (2.0 * (1.0 - kb) * s) + m.offset[1]));
            uv.push(code((r - lum) / (2.0 * (1.0 - kr) * s) + m.offset[2]));
        }
    }
    Nv12 {
        size: PixelSize::new(w, h),
        y,
        uv,
        y_stride: w,
        uv_stride: w,
        colour,
    }
}
#[test]
fn nv12_matches_cpu_reference() -> Result<()> {
    let Some(mut g) = gpu()? else { return Ok(()) };
    let mut seed = 123;
    for matrix in [YuvMatrix::Bt601, YuvMatrix::Bt709] {
        for full_range in [false, true] {
            for size in [
                PixelSize::new(1, 1),
                PixelSize::new(63, 65),
                PixelSize::new(130, 70),
            ] {
                let colour = YuvColour { matrix, full_range };
                for flat in [false, true] {
                    let mut bytes = random(size, &mut seed);
                    if flat {
                        // Flat 2x2 blocks permit round-trip RGB testing without chroma subsampling loss.
                        for y in 0..size.height {
                            for x in 0..size.width {
                                let src = ((y / 2 * 2) * size.width + x / 2 * 2) as usize * 4;
                                let dst = (y * size.width + x) as usize * 4;
                                let p = [bytes[src], bytes[src + 1], bytes[src + 2], 255];
                                bytes[dst..dst + 4].copy_from_slice(&p);
                            }
                        }
                    }
                    let expected = reference(&bytes, size, colour);
                    let w = expected.size.width;
                    let h = expected.size.height;
                    let pitch = w.next_multiple_of(4) + 8;
                    let offset = 272u64;
                    let uv_offset = offset + u64::from(pitch) * u64::from(h) + 16;
                    let buffer = g.device().create_buffer(&wgpu::BufferDescriptor {
                        label: None,
                        size: uv_offset + u64::from(pitch) * u64::from(h / 2) + 16,
                        usage: wgpu::BufferUsages::STORAGE
                            | wgpu::BufferUsages::COPY_SRC
                            | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    });
                    g.queue()
                        .write_buffer(&buffer, 0, &vec![0xA5; buffer.size() as usize]);
                    let t = upload(&g, size, &bytes);
                    g.scan(
                        region(&t),
                        Some(Nv12Output {
                            target: Nv12Target::Buffer {
                                buffer: &buffer,
                                y_offset: offset,
                                y_pitch: pitch,
                                uv_offset,
                                uv_pitch: pitch,
                            },
                            colour,
                        }),
                        false,
                    )?;
                    let staging = g.device().create_buffer(&wgpu::BufferDescriptor {
                        label: None,
                        size: buffer.size(),
                        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    });
                    let mut e = g.device().create_command_encoder(&Default::default());
                    e.copy_buffer_to_buffer(&buffer, 0, &staging, 0, buffer.size());
                    g.queue().submit([e.finish()]);
                    let raw = read_buffer(&g, &staging)?;
                    let plane = |start: u64, rows: u32| {
                        (0..rows)
                            .flat_map(|r| {
                                let i = (start + u64::from(r * pitch)) as usize;
                                raw[i..i + w as usize].iter().copied()
                            })
                            .collect::<Vec<_>>()
                    };
                    let actual = Nv12 {
                        y: plane(offset, h),
                        uv: plane(uv_offset, h / 2),
                        ..expected.clone()
                    };
                    for (a, b) in actual
                        .y
                        .iter()
                        .chain(&actual.uv)
                        .zip(expected.y.iter().chain(&expected.uv))
                    {
                        ensure!(a.abs_diff(*b) <= 1, "NV12 sample {a} vs {b}");
                    }
                    for (i, value) in raw.iter().enumerate() {
                        let used = [(offset, h), (uv_offset, h / 2)].iter().any(|(o, rows)| {
                            i as u64 >= *o
                                && (i as u64 - *o) / u64::from(pitch) < u64::from(*rows)
                                && (i as u64 - *o) % u64::from(pitch) < u64::from(w)
                        });
                        if !used {
                            ensure!(*value == 0xA5, "padding overwritten at {i}");
                        }
                    }
                    let mut roundtrip = Vec::new();
                    nv12_to_bgra(&actual, size, &mut roundtrip)?;
                    if flat {
                        for (a, b) in roundtrip.iter().zip(&bytes) {
                            ensure!(a.abs_diff(*b) <= 3, "RGB round trip {a} vs {b}");
                        }
                    }
                    if g.nv12_textures_supported() {
                        let y = texture(
                            &g,
                            expected.size,
                            wgpu::TextureFormat::R8Unorm,
                            wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
                        );
                        let uv = texture(
                            &g,
                            PixelSize::new(w / 2, h / 2),
                            wgpu::TextureFormat::Rg8Unorm,
                            wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
                        );
                        g.scan(
                            region(&t),
                            Some(Nv12Output {
                                target: Nv12Target::Textures { y: &y, uv: &uv },
                                colour,
                            }),
                            false,
                        )?;
                        let actual = Nv12 {
                            y: read_texture(&g, &y, 1)?,
                            uv: read_texture(&g, &uv, 2)?,
                            ..expected.clone()
                        };
                        for (a, b) in actual
                            .y
                            .iter()
                            .chain(&actual.uv)
                            .zip(expected.y.iter().chain(&expected.uv))
                        {
                            ensure!(a.abs_diff(*b) <= 1);
                        }
                        nv12_to_bgra(&actual, size, &mut roundtrip)?;
                        if flat {
                            for (a, b) in roundtrip.iter().zip(&bytes) {
                                ensure!(a.abs_diff(*b) <= 3, "texture RGB round trip {a} vs {b}");
                            }
                        }
                        let small = texture(
                            &g,
                            PixelSize::new(1, 1),
                            wgpu::TextureFormat::R8Unorm,
                            wgpu::TextureUsages::TEXTURE_BINDING,
                        );
                        ensure!(
                            g.scan(
                                region(&t),
                                Some(Nv12Output {
                                    target: Nv12Target::Textures { y: &small, uv: &uv },
                                    colour
                                }),
                                false
                            )
                            .is_err()
                        );
                    } else {
                        eprintln!("SKIP NV12 textures: R8/RG8 storage unavailable");
                    }
                    ensure!(
                        g.scan(
                            region(&t),
                            Some(Nv12Output {
                                target: Nv12Target::Buffer {
                                    buffer: &buffer,
                                    y_offset: 1,
                                    y_pitch: pitch,
                                    uv_offset,
                                    uv_pitch: pitch
                                },
                                colour
                            }),
                            false
                        )
                        .is_err()
                    );
                }
            }
        }
    }
    Ok(())
}
#[test]
fn timing_report() -> Result<()> {
    let Some(mut g) = gpu()? else { return Ok(()) };
    let size = PixelSize::new(3440, 1440);
    let mut seed = 71;
    let t = upload(&g, size, &random(size, &mut seed));
    let buffer = g.device().create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(size.width) * u64::from(size.height) * 3 / 2,
        usage: wgpu::BufferUsages::STORAGE,
        mapped_at_creation: false,
    });
    let output = Nv12Output {
        target: Nv12Target::Buffer {
            buffer: &buffer,
            y_offset: 0,
            y_pitch: size.width,
            uv_offset: u64::from(size.width) * u64::from(size.height),
            uv_pitch: size.width,
        },
        colour: YuvColour::default(),
    };
    let mut changes = g.scan(region(&t), None, false)?;
    g.commit();
    for nv in [None, Some(output)] {
        g.scan(region(&t), nv, false)?;
        let start = Instant::now();
        for _ in 0..10 {
            g.scan(region(&t), nv, false)?;
        }
        eprintln!(
            "3440x1440 scan NV12={}: {:.3} ms",
            nv.is_some(),
            start.elapsed().as_secs_f64() * 100.0
        );
    }
    for all in [false, true] {
        changes.changed_bits.fill(0);
        let n = (changes.tiles_x * changes.tiles_y).div_ceil(10);
        for i in 0..n {
            changes.changed_bits[(i / 32) as usize] |= 1 << (i % 32);
        }
        changes.changed = n;
        drop(g.gather(region(&t), &changes, all)?);
        let start = Instant::now();
        for _ in 0..10 {
            drop(g.gather(region(&t), &changes, all)?);
        }
        eprintln!(
            "3440x1440 gather {}%: {:.3} ms",
            if all { 100 } else { 10 },
            start.elapsed().as_secs_f64() * 100.0
        );
    }
    Ok(())
}
