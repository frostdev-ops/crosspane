//! Demo review captures without a window: the same shell, fonts, theme and art, drawn by
//! eframe's own wgpu renderer into an offscreen texture.
//!
//! Review only. It runs on a scripted clock (16 ms per frame), so a capture at a given time into a
//! transition is the same on every run, at any logical size and any pixels-per-point. It never
//! constructs a production controller.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use crosspane_ui_kit::{
    art::{Art, BrandBytes},
    theme,
};
use eframe::egui;
use eframe::egui_wgpu::{self, wgpu};

use crate::WizardShell;
use crate::demo::DisconnectedController;
use crate::gui::InstallerController;

/// One virtual frame of the scripted review clock.
pub const FRAME_MS: u64 = 16;
/// How long the `from` screen is shown before the reviewed screen replaces it.
pub const FROM_SETTLE_MS: u64 = 1_600;
/// A capture with no time given is taken this long after the reviewed screen appears.
pub const SETTLED_MS: u64 = 2_400;
/// The largest offscreen capture side, in pixels.
const MAX_SIDE: u32 = 8_192;

/// What a scripted review shows and when it is captured.
#[derive(Clone, Debug)]
pub struct ReviewScript {
    /// The screen or variant under review.
    pub screen: String,
    /// A screen shown first; the reviewed screen then replaces it, so its transition is seen.
    pub from: Option<String>,
    /// Time after the reviewed screen appears at which the capture is taken.
    pub at_ms: u64,
    /// Logical window size, in points.
    pub size: egui::Vec2,
    /// Physical pixels per logical point (2.0 matches a Retina display).
    pub pixels_per_point: f32,
    /// Where the pointer rests, in points, to review hover states.
    pub pointer: Option<egui::Pos2>,
}

/// The scripted clock: where a review is at a given frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScriptStep {
    /// Virtual milliseconds since the window opened.
    pub now_ms: u64,
    /// The reviewed screen is showing (otherwise the `from` screen is).
    pub target: bool,
    /// This is the frame to capture.
    pub capture: bool,
}

impl ReviewScript {
    /// The virtual time at which the reviewed screen replaces the `from` screen.
    pub fn switch_ms(&self) -> u64 {
        if self.from.is_some() {
            FROM_SETTLE_MS
        } else {
            0
        }
    }

    /// The step for frame `index` (0-based).
    pub fn step(&self, index: u64) -> ScriptStep {
        let end = self.switch_ms() + self.at_ms;
        let now_ms = (index * FRAME_MS).min(end);
        ScriptStep {
            now_ms,
            target: now_ms >= self.switch_ms(),
            capture: index * FRAME_MS >= end && index >= 3,
        }
    }
}

/// Builds the review controller for a screen or variant name.
pub fn controller(name: &str) -> Result<DisconnectedController> {
    DisconnectedController::named(name).with_context(|| format!("Unknown demo screen: {name}"))
}

/// Render `script` without a window and save the captured frame as a PNG.
pub fn render_offscreen(
    script: &ReviewScript,
    fonts: egui::FontDefinitions,
    brand: BrandBytes<'_>,
    path: &Path,
) -> Result<()> {
    let ppp = script.pixels_per_point;
    ensure!(
        (0.5..=4.0).contains(&ppp),
        "--pixels-per-point must be between 0.5 and 4"
    );
    let width = (script.size.x * ppp).round() as u32;
    let height = (script.size.y * ppp).round() as u32;
    ensure!(
        (16..=MAX_SIDE).contains(&width) && (16..=MAX_SIDE).contains(&height),
        "Review size out of range"
    );
    let ctx = egui::Context::default();
    ctx.set_fonts(fonts);
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_style_of(egui::Theme::Dark, theme::style());
    let art = Art::load(&ctx, brand);
    let mut shell = WizardShell::default();
    let mut from = script.from.as_deref().map(controller).transpose()?;
    let mut target = controller(&script.screen)?;

    let gpu = Gpu::new(width, height)?;
    let mut renderer = egui_wgpu::Renderer::new(
        &gpu.device,
        wgpu::TextureFormat::Rgba8Unorm,
        egui_wgpu::RendererOptions::default(),
    );
    let screen = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [width, height],
        pixels_per_point: ppp,
    };
    let mut index = 0;
    loop {
        let step = script.step(index);
        let mut events = Vec::new();
        if let Some(pointer) = script.pointer {
            events.push(egui::Event::PointerMoved(pointer));
        }
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, script.size)),
            time: Some(step.now_ms as f64 / 1000.0),
            events,
            focused: true,
            ..Default::default()
        };
        // The window's native scale: the context lays out in points and paints in pixels.
        ctx.set_pixels_per_point(ppp);
        let controller: &mut dyn InstallerController = match (&mut from, step.target) {
            (Some(from), false) => from,
            _ => &mut target,
        };
        let mut output = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| {
                    shell.show(ui, controller.view(), &art, step.now_ms);
                });
        });
        for (id, deltas) in &output.textures_delta.set {
            for delta in deltas {
                renderer.update_texture(&gpu.device, &gpu.queue, *id, delta);
            }
        }
        let freed = std::mem::take(&mut output.textures_delta.free);
        output.textures_delta.clear();
        if step.capture {
            let jobs = ctx.tessellate(output.shapes, output.pixels_per_point);
            let image = gpu.draw(&mut renderer, &jobs, &screen)?;
            return save_rgba(path, width, height, &image);
        }
        for id in &freed {
            renderer.free_texture(id);
        }
        if step.target {
            from = None;
        }
        index += 1;
        ensure!(index < 100_000, "Review script did not finish");
    }
}

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    texture: wgpu::Texture,
    width: u32,
    height: u32,
}

impl Gpu {
    fn new(width: u32, height: u32) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: None,
            ..Default::default()
        }))
        .context("No GPU adapter for offscreen review")?;
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .context("No GPU device for offscreen review")?;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("crosspane-review"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        Ok(Self {
            device,
            queue,
            texture,
            width,
            height,
        })
    }

    fn draw(
        &self,
        renderer: &mut egui_wgpu::Renderer,
        jobs: &[egui::ClippedPrimitive],
        screen: &egui_wgpu::ScreenDescriptor,
    ) -> Result<Vec<u8>> {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let extra = renderer.update_buffers(&self.device, &self.queue, &mut encoder, jobs, screen);
        let view = self
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("crosspane-review"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let mut pass = pass.forget_lifetime();
            renderer.render(&mut pass, jobs, screen);
        }
        let row = (self.width * 4).div_ceil(256) * 256;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("crosspane-review-readback"),
            size: u64::from(row) * u64::from(self.height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        self.queue
            .submit(extra.into_iter().chain(std::iter::once(encoder.finish())));
        let slice = buffer.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .context("GPU readback failed")?;
        receiver
            .recv()
            .context("GPU readback did not answer")?
            .context("GPU readback mapping failed")?;
        let data = slice.get_mapped_range().context("GPU readback range")?;
        let mut pixels = Vec::with_capacity(self.width as usize * self.height as usize * 4);
        for line in data.chunks(row as usize).take(self.height as usize) {
            pixels.extend_from_slice(&line[..self.width as usize * 4]);
        }
        Ok(pixels)
    }
}

fn save_rgba(path: &Path, width: u32, height: u32, pixels: &[u8]) -> Result<()> {
    let file =
        std::fs::File::create(path).with_context(|| format!("Cannot write {}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    // The texture is opaque where the backdrop draws; make the PNG opaque everywhere.
    let opaque: Vec<u8> = pixels
        .chunks(4)
        .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
        .collect();
    writer.write_image_data(&opaque)?;
    writer.finish()?;
    Ok(())
}

/// Drives a native wgpu future to completion; on native backends these resolve at once.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    loop {
        if let std::task::Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
        std::thread::yield_now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_switches_then_captures_at_the_requested_time() {
        let script = ReviewScript {
            screen: "connect".into(),
            from: Some("welcome".into()),
            at_ms: 160,
            size: egui::vec2(800.0, 600.0),
            pixels_per_point: 1.0,
            pointer: None,
        };
        assert!(!script.step(0).target);
        let switch = FROM_SETTLE_MS / FRAME_MS;
        assert!(script.step(switch).target);
        assert!(!script.step(switch).capture);
        let capture = script.step(switch + 10);
        assert!(capture.capture && capture.now_ms == FROM_SETTLE_MS + 160);
        let alone = ReviewScript {
            from: None,
            at_ms: 0,
            ..script
        };
        assert!(!alone.step(2).capture && alone.step(3).capture);
    }
}
