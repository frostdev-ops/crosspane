#![cfg(all(target_os = "macos", feature = "gpu"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crosspane_media::{
    codec::{CodecError, VideoCodecs, VideoDecoder, VideoEncoder},
    picture::{Decoded, NativePicture, Nv12, YuvColour},
};
use crosspane_platform_macos::{
    gpu_import::import_picture,
    video::{Decoder, VtCodecs},
};
use crosspane_types::geom::PixelSize;
use std::{
    sync::{Arc, mpsc},
    time::Duration,
};

fn available<T>(result: Result<T, CodecError>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(CodecError::Unavailable(reason)) => {
            eprintln!("SKIP: VideoToolbox session unavailable: {reason}");
            None
        }
        Err(error) => panic!("VideoToolbox: {error}"),
    }
}

fn fixture(size: PixelSize) -> Option<(Box<dyn VideoEncoder>, Decoder, Vec<u8>)> {
    let codecs = VtCodecs::new();
    let mut encoder = available(codecs.encoder(size, 20_000_000, 30))?;
    let mut decoder = available(codecs.nv12_decoder())?;
    let pixels: Vec<u8> = (0..size.width * size.height)
        .flat_map(|i| {
            [
                (i % 251) as u8,
                (i / 7 % 251) as u8,
                (i / 13 % 251) as u8,
                255,
            ]
        })
        .collect();
    let mut packet = Vec::new();
    encoder
        .encode(&pixels, size.width * 4, size, true, &mut packet)
        .unwrap();
    available(decoder.decode_native(&packet, &mut Arc::new(Nv12::default())))?;
    Some((encoder, decoder, packet))
}

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
    assert_eq!(adapter.get_info().backend, wgpu::Backend::Metal);
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

fn native(decoder: &mut Decoder, packet: &[u8]) -> Arc<dyn NativePicture> {
    match decoder
        .decode_native(packet, &mut Arc::new(Nv12::default()))
        .unwrap()
    {
        Decoded::Native(picture) => picture,
        Decoded::Nv12(_) => panic!("VT did not produce an even IOSurface-backed NV12 buffer"),
    }
}

// Imported textures deliberately have only TEXTURE_BINDING. textureLoad reads their exact
// samples into a storage buffer, avoiding an invalid COPY_SRC use or changing production usage.
fn read_plane(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: wgpu::Texture,
    components: usize,
) -> Vec<u8> {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: None,
        source: wgpu::ShaderSource::Wgsl(
            format!(
                r#"
            @group(0) @binding(0) var plane: texture_2d<f32>;
            @group(0) @binding(1) var<storage, read_write> output: array<u32>;
            @compute @workgroup_size(8,8)
            fn main(@builtin(global_invocation_id) id: vec3<u32>) {{
                let size = textureDimensions(plane);
                if (id.x >= size.x || id.y >= size.y) {{ return; }}
                let value = vec2<u32>(round(textureLoad(plane, vec2<i32>(id.xy), 0).rg * 255.0));
                let offset = (id.y * size.x + id.x) * {components}u;
                output[offset] = value.x;
                if ({components}u == 2u) {{ output[offset + 1u] = value.y; }}
            }}"#
            )
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
    let bytes = u64::from(texture.width()) * u64::from(texture.height()) * components as u64 * 4;
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
fn byte_exact_import() {
    let size = PixelSize::new(66, 34); // Exercise CoreVideo row padding.
    let Some((mut encoder, mut decoder, mut packet)) = fixture(size) else {
        return;
    };
    let Some((device, queue)) = gpu() else {
        return;
    };
    for frame in 0..4 {
        let picture = native(&mut decoder, &packet);
        let mut cpu = Nv12::default();
        picture.to_nv12(&mut cpu).unwrap();
        assert_eq!(picture.colour(), cpu.colour);
        eprintln!(
            "VT format={}, IOSurface-backed=true",
            if cpu.colour.full_range {
                "420f"
            } else {
                "420v"
            }
        );
        let textures = import_picture(&device, picture.as_ref()).unwrap();
        for (plane, (texture, (bytes, stride))) in textures
            .into_iter()
            .zip([(&cpu.y, cpu.y_stride), (&cpu.uv, cpu.uv_stride)])
            .enumerate()
        {
            let packed: Vec<_> = bytes
                .chunks_exact(stride as usize)
                .flat_map(|row| row[..size.width as usize].iter().copied())
                .collect();
            assert_eq!(read_plane(&device, &queue, texture, plane + 1), packed);
        }
        let pixels = [frame * 37, 150, 208, 255].repeat(size.width as usize * size.height as usize);
        encoder
            .encode(&pixels, size.width * 4, size, false, &mut packet)
            .unwrap();
    }
}

#[derive(Debug)]
struct Foreign;
impl NativePicture for Foreign {
    fn size(&self) -> PixelSize {
        PixelSize::new(2, 2)
    }
    fn colour(&self) -> YuvColour {
        YuvColour::default()
    }
    fn to_nv12(&self, _: &mut Nv12) -> Result<(), CodecError> {
        Err(CodecError::BadInput("test"))
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[test]
fn foreign_picture_is_refused() {
    let Some((device, _)) = gpu() else {
        return;
    };
    assert!(!import_picture(&device, &Foreign).unwrap_err().is_empty());
}

#[test]
fn midstream_output_change() {
    let size = PixelSize::new(32, 32);
    let Some((mut encoder, mut decoder, mut packet)) = fixture(size) else {
        return;
    };
    let pixels = [48, 150, 208, 255].repeat(32 * 32);
    let mut bgra = Vec::new();
    decoder.decode(&packet, &mut bgra).unwrap();
    encoder
        .encode(&pixels, 128, size, false, &mut packet)
        .unwrap();
    assert!(matches!(
        decoder
            .decode_native(&packet, &mut Arc::new(Nv12::default()))
            .unwrap(),
        Decoded::Nv12(_)
    ));
    encoder
        .encode(&pixels, 128, size, true, &mut packet)
        .unwrap();
    let picture = native(&mut decoder, &packet);
    picture.to_nv12(&mut Nv12::default()).unwrap();
    encoder
        .encode(&pixels, 128, size, false, &mut packet)
        .unwrap();
    decoder.decode(&packet, &mut bgra).unwrap();
    encoder
        .encode(&pixels, 128, size, true, &mut packet)
        .unwrap();
    decoder.decode(&packet, &mut bgra).unwrap();
    encoder
        .encode(&pixels, 128, size, false, &mut packet)
        .unwrap();
    assert!(matches!(
        decoder
            .decode_native(&packet, &mut Arc::new(Nv12::default()))
            .unwrap(),
        Decoded::Nv12(_)
    ));
}

#[repr(C)]
#[derive(Default)]
struct Timeval {
    seconds: std::ffi::c_long,
    microseconds: std::ffi::c_int,
}
#[repr(C)]
#[derive(Default)]
struct Rusage {
    user: Timeval,
    system: Timeval,
    counters: [std::ffi::c_long; 14],
}
unsafe extern "C" {
    fn getrusage(who: std::ffi::c_int, usage: *mut Rusage) -> std::ffi::c_int;
}
fn usage() -> Rusage {
    let mut value = Rusage::default();
    // SAFETY: Darwin's public rusage layout and writable stack storage; RUSAGE_SELF=0.
    assert_eq!(unsafe { getrusage(0, &mut value) }, 0);
    value
}
fn cpu_seconds() -> f64 {
    let value = usage();
    (value.user.seconds + value.system.seconds) as f64
        + f64::from(value.user.microseconds + value.system.microseconds) / 1_000_000.0
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
fn release_1000_cycles() {
    let Some((_, mut decoder, packet)) = fixture(PixelSize::new(1920, 1080)) else {
        return;
    };
    let Some((device, queue)) = gpu() else {
        return;
    };
    for _ in 0..20 {
        drop(import_picture(&device, native(&mut decoder, &packet).as_ref()).unwrap());
        wait(&device);
    }
    let warm_picture = native(&mut decoder, &packet);
    let [warm_y, warm_uv] = import_picture(&device, warm_picture.as_ref()).unwrap();
    assert!(!read_plane(&device, &queue, warm_y, 1).is_empty());
    drop(warm_uv);
    drop(warm_picture);
    wait(&device);
    let before = resident_bytes();
    for _ in 0..1000 {
        let picture = native(&mut decoder, &packet);
        let textures = import_picture(&device, picture.as_ref()).unwrap();
        drop(textures);
        wait(&device);
        // The importer borrows this outer Arc; the callbacks share a separate CoreVideo
        // retain. This assertion checks caller ownership, while RSS and continued decode
        // check recycling; it alone cannot prove that the callbacks ran.
        assert_eq!(Arc::strong_count(&picture), 1);
    }
    let growth = resident_bytes().saturating_sub(before);
    eprintln!("resident growth over 1000 cycles: {growth} bytes");
    assert!(growth <= 32 * 1024 * 1024);
}

#[test]
fn cpu_time_1080p() {
    let Some((_, mut decoder, packet)) = fixture(PixelSize::new(1920, 1080)) else {
        return;
    };
    let Some((device, _)) = gpu() else {
        return;
    };
    let mut reuse = Arc::new(Nv12::default());
    let mut cpu = Nv12::default();
    decoder.decode_nv12(&packet, &mut cpu).unwrap();
    drop(import_picture(&device, native(&mut decoder, &packet).as_ref()).unwrap());
    wait(&device);
    let start = cpu_seconds();
    for _ in 0..120 {
        decoder.decode_nv12(&packet, &mut cpu).unwrap();
    }
    let copied = (cpu_seconds() - start) / 120.0;
    let mut imported = 0.0;
    for _ in 0..120 {
        let start = cpu_seconds();
        let Decoded::Native(picture) = decoder.decode_native(&packet, &mut reuse).unwrap() else {
            panic!("expected native picture")
        };
        drop(import_picture(&device, picture.as_ref()).unwrap());
        drop(picture);
        imported += cpu_seconds() - start;
        wait(&device);
    }
    let imported = imported / 120.0;
    eprintln!("1080p CPU/frame: decode_nv12={copied:.6}s; decode_native+import={imported:.6}s");
}
