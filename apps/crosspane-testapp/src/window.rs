use std::{path::Path, sync::Arc, time::Instant};

use anyhow::{Context, Result, bail, ensure};
use crosspane_testapp::{gpu::PatternRenderer, pattern::ImageSize};
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalSize, PhysicalSize},
    event::{Ime, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::PhysicalKey,
    window::{Window, WindowId},
};

use crate::events::{Event, EventLog, state_name};

pub(crate) fn run(
    title: String,
    size: ImageSize,
    events: Option<&Path>,
    start: Instant,
) -> Result<()> {
    let log = EventLog::new(events, start)?;
    let event_loop = EventLoop::new().context("creating the window event loop")?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App {
        title,
        logical_size: size,
        log,
        window: None,
        failure: None,
        ready: false,
    };
    event_loop.run_app(&mut app)?;
    match app.failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

struct App {
    title: String,
    logical_size: ImageSize,
    log: EventLog,
    window: Option<FixtureWindow>,
    failure: Option<anyhow::Error>,
    ready: bool,
}

impl App {
    fn fail(&mut self, event_loop: &ActiveEventLoop, error: anyhow::Error) {
        self.failure = Some(error);
        event_loop.exit();
    }

    fn handle_event(&mut self, event_loop: &ActiveEventLoop, event: WindowEvent) -> Result<()> {
        let Some(window) = &mut self.window else {
            return Ok(());
        };
        match event {
            WindowEvent::RedrawRequested => {
                if window.render()? && !self.ready {
                    self.log.write(Event::Ready {
                        scale: window.window.scale_factor(),
                        size_px: [window.size.width, window.size.height],
                    })?;
                    self.ready = true;
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let code = match event.physical_key {
                    PhysicalKey::Code(code) => format!("{code:?}"),
                    PhysicalKey::Unidentified(_) => "Unidentified".to_owned(),
                };
                self.log.write(Event::Key {
                    code: &code,
                    state: state_name(event.state),
                    repeat: event.repeat,
                    text: event.text.as_deref(),
                })?;
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                self.log.write(Event::ImeCommit { text: &text })?
            }
            WindowEvent::CursorMoved { position, .. } => self.log.write(Event::Pointer {
                x_px: position.x,
                y_px: position.y,
            })?,
            WindowEvent::MouseInput { state, button, .. } => {
                let button = self.log.button(button)?;
                self.log.write(Event::Button {
                    button,
                    state: state_name(state),
                })?;
            }
            WindowEvent::MouseWheel { delta, .. } => self.log.write(match delta {
                MouseScrollDelta::LineDelta(x, y) => Event::WheelLines {
                    lines_x: f64::from(x),
                    lines_y: f64::from(y),
                },
                MouseScrollDelta::PixelDelta(position) => Event::WheelPixels {
                    px_x: position.x,
                    px_y: position.y,
                },
            })?,
            WindowEvent::Focused(focused) => self.log.write(Event::Focus { focused })?,
            WindowEvent::Resized(size) => {
                self.log.write(Event::Resize {
                    size_px: [size.width, size.height],
                })?;
                window.resize(size)?;
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.log.write(Event::Scale {
                    scale: scale_factor,
                })?;
                // Winit also emits Resized with the final device-pixel dimensions.
                window.window.request_redraw();
            }
            WindowEvent::CloseRequested => {
                self.log.write(Event::Close)?;
                event_loop.exit();
            }
            _ => {}
        }
        Ok(())
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() || self.failure.is_some() {
            return;
        }
        match FixtureWindow::new(event_loop, &self.title, self.logical_size) {
            Ok(window) => self.window = Some(window),
            Err(error) => self.fail(event_loop, error),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if self.failure.is_some() || self.window.as_ref().is_none_or(|w| w.window.id() != id) {
            return;
        }
        if let Err(error) = self.handle_event(event_loop, event) {
            self.fail(event_loop, error);
        }
    }
}

struct FixtureWindow {
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    window: Arc<Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    renderer: PatternRenderer,
    config: wgpu::SurfaceConfiguration,
    view_format: wgpu::TextureFormat,
    size: PhysicalSize<u32>,
}

impl FixtureWindow {
    fn new(event_loop: &ActiveEventLoop, title: &str, logical_size: ImageSize) -> Result<Self> {
        let window = Arc::new(
            event_loop.create_window(
                Window::default_attributes()
                    .with_title(title)
                    .with_inner_size(LogicalSize::new(
                        logical_size.width(),
                        logical_size.height(),
                    )),
            )?,
        );
        window.set_ime_allowed(true);
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
            Box::new(event_loop.owned_display_handle()),
        ));
        let surface = instance.create_surface(window.clone())?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            power_preference: wgpu::PowerPreference::None,
            apply_limit_buckets: false,
        }))
        .context("finding a GPU adapter for the fixture window")?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("fixture window"),
                ..Default::default()
            }))?;
        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        let format = surface_format(&caps.formats).context("surface has no supported formats")?;
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .context("surface has no supported configuration")?;
        config.format = format;
        // An sRGB-only surface still allows a non-sRGB view, preserving the pattern's bytes.
        let view_format = format.remove_srgb_suffix();
        if view_format != format {
            config.view_formats = vec![view_format];
        }
        eprintln!("fixture surface format: {format:?}; render view: {view_format:?}");
        let renderer = PatternRenderer::new(&device, view_format);
        let mut fixture = Self {
            instance,
            surface,
            window,
            device,
            queue,
            renderer,
            config,
            view_format,
            size,
        };
        fixture.resize(size)?;
        Ok(fixture)
    }

    fn resize(&mut self, size: PhysicalSize<u32>) -> Result<()> {
        self.size = size;
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        let limit = self.device.limits().max_texture_dimension_2d;
        ensure!(
            size.width <= limit && size.height <= limit,
            "window exceeds GPU texture limits"
        );
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
        self.window.request_redraw();
        Ok(())
    }

    fn render(&mut self) -> Result<bool> {
        if self.size.width == 0 || self.size.height == 0 {
            return Ok(false);
        }
        let (frame, suboptimal) = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => (frame, false),
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => (frame, true),
            wgpu::CurrentSurfaceTexture::Timeout => {
                self.window.request_redraw();
                return Ok(false);
            }
            wgpu::CurrentSurfaceTexture::Occluded => return Ok(false),
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.resize(self.window.inner_size())?;
                return Ok(false);
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.surface = self.instance.create_surface(self.window.clone())?;
                self.resize(self.window.inner_size())?;
                return Ok(false);
            }
            wgpu::CurrentSurfaceTexture::Validation => bail!("fixture surface validation failed"),
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(self.view_format),
            ..Default::default()
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("fixture window draw"),
            });
        self.renderer.encode(
            &self.queue,
            &mut encoder,
            &view,
            ImageSize::new(self.size.width, self.size.height)?,
        );
        self.queue.submit([encoder.finish()]);
        self.window.pre_present_notify();
        self.queue.present(frame);
        if suboptimal {
            self.resize(self.window.inner_size())?;
        }
        Ok(true)
    }
}

fn surface_format(formats: &[wgpu::TextureFormat]) -> Option<wgpu::TextureFormat> {
    use wgpu::TextureFormat::{Bgra8Unorm, Bgra8UnormSrgb, Rgba8Unorm, Rgba8UnormSrgb};

    [Rgba8Unorm, Bgra8Unorm]
        .into_iter()
        .find(|format| formats.contains(format))
        .or_else(|| formats.iter().copied().find(|format| !format.is_srgb()))
        .or_else(|| {
            [Rgba8UnormSrgb, Bgra8UnormSrgb]
                .into_iter()
                .find(|format| formats.contains(format))
        })
        .or_else(|| formats.first().copied())
}
