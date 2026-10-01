use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crosspane_types::geom::{PixelSize, PointDevice};
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalSize, PhysicalSize},
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy},
    keyboard::PhysicalKey,
    monitor::MonitorHandle,
    window::{CursorIcon, CustomCursor, Window, WindowId},
};

use super::{
    HostCommand, HostEvent, PictureImporter, cursor,
    gpu::{Presenter, surface_format},
    input::{InputState, mouse_button, scroll},
};
use crate::keymap::keycode_to_hid;

pub(super) struct App {
    proxy: EventLoopProxy<HostCommand>,
    events: Box<dyn FnMut(HostEvent)>,
    instance: Option<wgpu::Instance>,
    gpu: Option<Gpu>,
    windows: HashMap<u64, ProxyWindow>,
    ids: HashMap<WindowId, u64>,
    pending: VecDeque<HostCommand>,
    importer: Option<PictureImporter>,
}

struct Gpu {
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    failed: Arc<AtomicBool>,
}

struct ProxyWindow {
    surface: wgpu::Surface<'static>,
    window: Arc<Window>,
    presenter: Presenter,
    config: wgpu::SurfaceConfiguration,
    size: PhysicalSize<u32>,
    scale: f64,
    accent: [u8; 3],
    fullscreen: bool,
    flash_until: Option<Instant>,
    input: InputState,
    consecutive_surface_losses: u8,
}

impl App {
    pub(super) fn new(
        proxy: EventLoopProxy<HostCommand>,
        events: Box<dyn FnMut(HostEvent)>,
        importer: Option<PictureImporter>,
    ) -> Self {
        Self {
            importer,
            proxy,
            events,
            instance: None,
            gpu: None,
            windows: HashMap::new(),
            ids: HashMap::new(),
            pending: VecDeque::new(),
        }
    }

    fn open(
        &mut self,
        event_loop: &ActiveEventLoop,
        id: u64,
        title: String,
        size: PixelSize,
        accent: [u8; 3],
    ) -> Result<(), String> {
        if self.windows.contains_key(&id) {
            return Err("proxy ID is already open".into());
        }
        if size.width == 0 || size.height == 0 {
            return Err("content size must be nonzero".into());
        }
        let instance = self.instance.as_ref().ok_or("event loop has not resumed")?;
        let monitor = event_loop
            .primary_monitor()
            .or_else(|| event_loop.available_monitors().next());
        let initial_scale = monitor
            .as_ref()
            .map_or(1.0, |monitor| monitor.scale_factor());
        let attributes = Window::default_attributes()
            .with_title(title)
            .with_decorations(true)
            .with_resizable(true)
            .with_inner_size(fit(
                logical_size(size, initial_scale),
                screen(monitor.as_ref()),
            ));
        #[cfg(target_os = "linux")]
        let attributes = {
            use winit::platform::wayland::WindowAttributesExtWayland;
            attributes.with_name("crosspane-proxy", "Crosspane")
        };
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .map_err(|error| error.to_string())?,
        );
        window.set_ime_allowed(false);
        let scale = window.scale_factor();
        if scale != initial_scale {
            let _ = window.request_inner_size(fit(
                logical_size(size, scale),
                screen(window.current_monitor().as_ref()),
            ));
        }
        let surface = instance
            .create_surface(window.clone())
            .map_err(|error| error.to_string())?;
        if self.gpu.is_none() {
            self.gpu = Some(Gpu::new(instance, &surface, self.proxy.clone())?);
        }
        let gpu = self.gpu.as_ref().ok_or("GPU initialization failed")?;
        if gpu.failed.load(Ordering::Acquire) {
            return Err("shared GPU has failed".into());
        }
        let caps = surface.get_capabilities(&gpu.adapter);
        let format =
            surface_format(&caps.formats).ok_or("surface has no byte-exact 8-bit format")?;
        tracing::info!(?format, formats = ?caps.formats, adapter = ?gpu.adapter.get_info(), "proxy surface format");
        let actual_size = window.inner_size();
        let mut config = surface
            .get_default_config(
                &gpu.adapter,
                actual_size.width.max(1),
                actual_size.height.max(1),
            )
            .ok_or("surface has no supported configuration")?;
        if !caps.present_modes.contains(&wgpu::PresentMode::Fifo) {
            return Err("surface does not support FIFO presentation".into());
        }
        config.format = format;
        config.present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };
        config.desired_maximum_frame_latency = 1;
        tracing::debug!(id, mode = ?config.present_mode, "proxy present mode");
        // Opaque avoids compositor alpha blending of a source window's canvas where supported.
        if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
            config.alpha_mode = wgpu::CompositeAlphaMode::Opaque;
        }
        let presenter = Presenter::new(&gpu.device, format);
        let mut proxy = ProxyWindow {
            surface,
            window,
            presenter,
            config,
            size: actual_size,
            scale,
            accent,
            fullscreen: false,
            flash_until: None,
            input: InputState::default(),
            consecutive_surface_losses: 0,
        };
        proxy.resize(gpu, actual_size)?;
        if gpu.failed.load(Ordering::Acquire) {
            return Err("GPU failed while opening proxy".into());
        }
        self.ids.insert(proxy.window.id(), id);
        self.windows.insert(id, proxy);
        (self.events)(HostEvent::Opened {
            id,
            size: pixel_size(actual_size),
            scale,
        });
        Ok(())
    }

    fn remove(&mut self, id: u64, lost: bool) {
        if let Some(mut window) = self.windows.remove(&id) {
            self.ids.remove(&window.window.id());
            window.input.release(id, self.events.as_mut());
            if lost {
                (self.events)(HostEvent::Lost { id });
            }
        }
    }

    fn check_gpu(&mut self) {
        if let Some(gpu) = &self.gpu
            && gpu.device.poll(wgpu::PollType::Poll).is_err()
        {
            gpu.failed.store(true, Ordering::Release);
        }
        if self
            .gpu
            .as_ref()
            .is_some_and(|gpu| gpu.failed.load(Ordering::Acquire))
        {
            let ids: Vec<_> = self.windows.keys().copied().collect();
            for id in ids {
                self.remove(id, true);
            }
        }
    }

    fn command(&mut self, event_loop: &ActiveEventLoop, command: HostCommand) {
        self.check_gpu();
        match command {
            HostCommand::Open {
                id,
                title,
                size,
                accent,
            } => {
                if let Err(error) = self.open(event_loop, id, title, size, accent) {
                    (self.events)(HostEvent::OpenFailed { id, error });
                }
            }
            HostCommand::Close { id } => self.remove(id, false),
            HostCommand::SetTitle { id, title } => {
                if let Some(window) = self.windows.get(&id) {
                    window.window.set_title(&title);
                }
            }
            HostCommand::SetContentSize { id, size } => {
                if size.width == 0 || size.height == 0 {
                    return;
                }
                if let Some(window) = self.windows.get_mut(&id) {
                    // The source's size is exact: no opening fit and no logical rounding, or the
                    // window system's answer would differ from it and be sent back as a resize.
                    let result = window.window.request_inner_size(content_request(size));
                    if let Some(actual) = result {
                        self.resized(id, actual, window_scale(&self.windows, id));
                    }
                }
            }
            HostCommand::Frame {
                id,
                size,
                pixels,
                dirty,
            } => {
                if let (Some(gpu), Some(window)) = (&self.gpu, self.windows.get_mut(&id)) {
                    match window
                        .presenter
                        .upload(&gpu.device, &gpu.queue, size, &pixels, &dirty)
                    {
                        Ok(()) => window.window.request_redraw(),
                        Err(error) => {
                            tracing::warn!(id, %error, "proxy frame rejected");
                            self.remove(id, true);
                        }
                    }
                }
            }
            HostCommand::Video {
                id,
                size,
                rect,
                picture,
            } => {
                if let (Some(gpu), Some(window)) = (&self.gpu, self.windows.get_mut(&id)) {
                    match window.presenter.set_video(&gpu.device, size, rect, picture) {
                        Ok(()) => window.window.request_redraw(),
                        Err(error) => {
                            tracing::warn!(id, %error, "proxy frame rejected");
                            self.remove(id, true);
                        }
                    }
                }
            }
            HostCommand::VideoNative {
                id,
                size,
                rect,
                picture,
            } => {
                if let (Some(gpu), Some(window)) = (&self.gpu, self.windows.get_mut(&id)) {
                    match window
                        .presenter
                        .set_native_video(&gpu.device, size, rect, picture)
                    {
                        Ok(()) => window.window.request_redraw(),
                        Err(error) => {
                            tracing::warn!(id, %error, "proxy frame rejected");
                            self.remove(id, true);
                        }
                    }
                }
            }
            HostCommand::SetCursor {
                id,
                size,
                hotspot,
                pixels,
            } => {
                if let Some(window) = self.windows.get(&id) {
                    set_cursor(event_loop, &window.window, size, hotspot, &pixels);
                }
            }
            HostCommand::DefaultCursor { id } => {
                if let Some(window) = self.windows.get(&id) {
                    window.window.set_cursor(CursorIcon::Default);
                    window.window.set_cursor_visible(true);
                }
            }
            HostCommand::Shutdown => {
                let ids: Vec<_> = self.windows.keys().copied().collect();
                for id in ids {
                    self.remove(id, false);
                }
                self.pending.clear();
                event_loop.exit();
            }
            HostCommand::Run(function) => function(),
        }
        self.check_gpu();
    }

    fn geometry_changed(&mut self, id: u64, report: Report) {
        if report.stale {
            tracing::debug!(id, size = ?report.size, scale = report.scale, "window event payload was stale");
        }
        self.resized(id, report.size, report.scale);
    }

    fn resized(&mut self, id: u64, size: PhysicalSize<u32>, scale: f64) {
        let (Some(gpu), Some(window)) = (&self.gpu, self.windows.get_mut(&id)) else {
            return;
        };
        let fullscreen = window.window.fullscreen().is_some();
        if fullscreen && !window.fullscreen {
            window.flash_until = Some(Instant::now() + Duration::from_secs(2));
        }
        window.fullscreen = fullscreen;
        window.scale = scale;
        if let Err(error) = window.resize(gpu, size) {
            tracing::warn!(id, %error, "proxy resize failed");
            self.remove(id, true);
            return;
        }
        (self.events)(HostEvent::Resized {
            id,
            size: pixel_size(size),
            scale,
        });
    }
}

fn set_cursor(
    event_loop: &ActiveEventLoop,
    window: &Window,
    size: PixelSize,
    hotspot: (u32, u32),
    pixels: &[u8],
) {
    match cursor::shape(
        size.width,
        size.height,
        hotspot,
        pixels,
        window.scale_factor(),
    ) {
        // The pointer over a proxy is this machine's own: it never disappears. A hidden remote
        // cursor (an app hides it while typing until the mouse moves, which injected motion may
        // not count as) shows the default arrow instead.
        Some(cursor::Shape::Hidden) => {
            window.set_cursor(CursorIcon::Default);
            window.set_cursor_visible(true);
        }
        Some(cursor::Shape::Image(image)) => {
            let source = CustomCursor::from_rgba(
                image.rgba,
                image.width,
                image.height,
                image.hotspot.0,
                image.hotspot.1,
            );
            match source {
                Ok(source) => {
                    window.set_cursor(event_loop.create_custom_cursor(source));
                    window.set_cursor_visible(true);
                }
                Err(error) => tracing::debug!(%error, "cursor image refused"),
            }
        }
        None => tracing::debug!("malformed cursor image"),
    }
}

impl ApplicationHandler<HostCommand> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.instance.is_none() {
            self.instance = Some(wgpu::Instance::new(
                wgpu::InstanceDescriptor::new_with_display_handle(Box::new(
                    event_loop.owned_display_handle(),
                )),
            ));
        }
        while let Some(command) = self.pending.pop_front() {
            self.command(event_loop, command);
            if event_loop.exiting() {
                break;
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: HostCommand) {
        if self.instance.is_none() {
            self.pending.push_back(event);
        } else {
            self.command(event_loop, event);
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        self.check_gpu();
        let Some(&id) = self.ids.get(&window_id) else {
            return;
        };
        let Some(window) = self.windows.get_mut(&id) else {
            return;
        };
        match event {
            WindowEvent::KeyboardInput { event, .. } if !event.repeat => {
                if let PhysicalKey::Code(code) = event.physical_key
                    && let Some(usage) = keycode_to_hid(code)
                {
                    window.input.key(usage, event.state);
                    (self.events)(HostEvent::Key {
                        id,
                        usage,
                        down: event.state.is_pressed(),
                    });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(button) = mouse_button(button) {
                    window.input.button(button, state);
                    (self.events)(HostEvent::Button {
                        id,
                        button,
                        down: state.is_pressed(),
                        position: window.input.position,
                    });
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                window.input.position = PointDevice::new(position.x, position.y);
                (self.events)(HostEvent::Motion {
                    id,
                    position: window.input.position,
                });
            }
            WindowEvent::MouseWheel { delta, phase, .. } => {
                (self.events)(HostEvent::Scroll {
                    id,
                    delta: scroll(delta, phase, window.scale),
                    position: window.input.position,
                });
            }
            WindowEvent::Focused(focused) => {
                if !focused {
                    window.input.release(id, self.events.as_mut());
                }
                (self.events)(HostEvent::Focus { id, focused });
            }
            // What is reported is the window as it is now, never the event's own payload: winit's
            // macOS `Resized` is a frame snapshot taken when the event was queued, and delivery
            // can come after later native changes.
            WindowEvent::Resized(size) => {
                let report = reported(Change::Resized(size), sample(&window.window));
                self.geometry_changed(id, report);
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let report = reported(Change::ScaleFactor(scale_factor), sample(&window.window));
                self.geometry_changed(id, report);
            }
            WindowEvent::CloseRequested => (self.events)(HostEvent::CloseRequested { id }),
            WindowEvent::Destroyed => self.remove(id, true),
            WindowEvent::RedrawRequested => {
                if let (Some(gpu), Some(instance)) = (&self.gpu, &self.instance)
                    && let Err(error) = window.render(gpu, instance, self.importer.as_deref())
                {
                    tracing::warn!(id, %error, "proxy rendering failed");
                    self.remove(id, true);
                }
            }
            _ => {}
        }
        self.check_gpu();
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.check_gpu();
        let now = Instant::now();
        let mut next = None;
        for window in self.windows.values_mut() {
            if let Some(deadline) = window.flash_until {
                if deadline <= now {
                    window.flash_until = None;
                    window.window.request_redraw();
                } else {
                    next = Some(next.map_or(deadline, |previous: Instant| previous.min(deadline)));
                }
            }
        }
        event_loop.set_control_flow(next.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }
}

impl Gpu {
    fn new(
        instance: &wgpu::Instance,
        surface: &wgpu::Surface<'_>,
        proxy: EventLoopProxy<HostCommand>,
    ) -> Result<Self, String> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(surface),
            force_fallback_adapter: false,
            power_preference: wgpu::PowerPreference::None,
            apply_limit_buckets: false,
        }))
        .map_err(|error| error.to_string())?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("shared proxy device"),
            ..Default::default()
        }))
        .map_err(|error| error.to_string())?;
        let failed = Arc::new(AtomicBool::new(false));
        let error_flag = failed.clone();
        let error_proxy = proxy.clone();
        device.on_uncaptured_error(Arc::new(move |_error| {
            error_flag.store(true, Ordering::Release);
            let _ = error_proxy.send_event(HostCommand::Run(Box::new(|| {})));
        }));
        let lost_flag = failed.clone();
        device.set_device_lost_callback(move |_reason, _message| {
            lost_flag.store(true, Ordering::Release);
            let _ = proxy.send_event(HostCommand::Run(Box::new(|| {})));
        });
        Ok(Self {
            adapter,
            device,
            queue,
            failed,
        })
    }
}

impl ProxyWindow {
    fn resize(&mut self, gpu: &Gpu, size: PhysicalSize<u32>) -> Result<(), String> {
        self.size = size;
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        let limit = gpu.device.limits().max_texture_dimension_2d;
        if size.width > limit || size.height > limit {
            return Err("window exceeds GPU limits".into());
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&gpu.device, &self.config);
        self.window.request_redraw();
        Ok(())
    }

    fn render(
        &mut self,
        gpu: &Gpu,
        instance: &wgpu::Instance,
        import: Option<super::gpu::Import<'_>>,
    ) -> Result<(), String> {
        if self.size.width == 0 || self.size.height == 0 {
            return Ok(());
        }
        let (frame, suboptimal) = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => (frame, false),
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => (frame, true),
            wgpu::CurrentSurfaceTexture::Timeout => {
                self.window.request_redraw();
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Occluded => return Ok(()),
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.resize(gpu, self.window.inner_size())?;
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.consecutive_surface_losses += 1;
                if self.consecutive_surface_losses > 3 {
                    return Err("surface remains lost after three recreations".into());
                }
                self.surface = instance
                    .create_surface(self.window.clone())
                    .map_err(|error| error.to_string())?;
                self.resize(gpu, self.window.inner_size())?;
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err("surface validation failed".into());
            }
        };
        self.presenter
            .prepare_video(&gpu.device, &gpu.queue, import);
        self.consecutive_surface_losses = 0;
        let view = frame.texture.create_view(&Default::default());
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("proxy draw"),
            });
        let normal = (2.0 * self.scale).round().max(1.0) as u32;
        let flashing = self
            .flash_until
            .is_some_and(|deadline| deadline > Instant::now());
        self.presenter.set_edge(
            self.accent,
            if flashing {
                normal.saturating_mul(4)
            } else {
                normal
            },
        );
        self.presenter
            .encode(&gpu.queue, pixel_size(self.size), &mut encoder, &view);
        gpu.queue.submit([encoder.finish()]);
        self.window.pre_present_notify();
        gpu.queue.present(frame);
        if suboptimal {
            self.resize(gpu, self.window.inner_size())?;
        }
        Ok(())
    }
}

fn pixel_size(size: PhysicalSize<u32>) -> PixelSize {
    PixelSize::new(size.width, size.height)
}

/// A window event saying the window's geometry changed, as winit delivers it.
#[derive(Clone, Copy, Debug)]
enum Change {
    /// `WindowEvent::Resized`: the size when the event was queued.
    Resized(PhysicalSize<u32>),
    /// `WindowEvent::ScaleFactorChanged`: the scale when the event was queued.
    ScaleFactor(f64),
}

/// What a geometry event reports to the engine.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Report {
    size: PhysicalSize<u32>,
    scale: f64,
    /// The event's own payload disagreed with the window as it is now.
    stale: bool,
}

/// The window's actual content size and scale, sampled together.
fn sample(window: &Window) -> (PhysicalSize<u32>, f64) {
    (window.inner_size(), window.scale_factor())
}

/// What a geometry event reports: the window's `actual` size and scale, sampled together when
/// the event is handled. The event's own payload only says that something changed; it may be
/// older than later native changes, and reporting it would tell the engine the user resized the
/// proxy to a size it no longer has.
fn reported(change: Change, actual: (PhysicalSize<u32>, f64)) -> Report {
    let stale = match change {
        Change::Resized(size) => size != actual.0,
        Change::ScaleFactor(scale) => scale != actual.1,
    };
    Report {
        size: actual.0,
        scale: actual.1,
        stale,
    }
}

/// The size to request for a `SetContentSize`: the exact physical content size. Only the opening
/// size is fitted to the screen (`fit`); the size the source reports back is applied as it is.
fn content_request(size: PixelSize) -> PhysicalSize<u32> {
    PhysicalSize::new(size.width, size.height)
}
fn logical_size(size: PixelSize, scale: f64) -> LogicalSize<f64> {
    LogicalSize::new(
        f64::from(size.width) / scale,
        f64::from(size.height) / scale,
    )
}
/// A monitor's size in logical units.
fn screen(monitor: Option<&MonitorHandle>) -> Option<LogicalSize<f64>> {
    monitor.map(|monitor| monitor.size().to_logical(monitor.scale_factor()))
}

/// `size`, scaled down (keeping its shape) to fit comfortably on `screen`. Proxies start at the
/// source window's size, and a window from an ultrawide monitor would otherwise open wider than a
/// laptop's display.
fn fit(size: LogicalSize<f64>, screen: Option<LogicalSize<f64>>) -> LogicalSize<f64> {
    let Some(screen) = screen.filter(|s| s.width > 0.0 && s.height > 0.0) else {
        return size;
    };
    let shrink = (screen.width * 0.9 / size.width)
        .min(screen.height * 0.8 / size.height)
        .min(1.0);
    LogicalSize::new(
        (size.width * shrink).floor().max(1.0),
        (size.height * shrink).floor().max(1.0),
    )
}

fn window_scale(windows: &HashMap<u64, ProxyWindow>, id: u64) -> f64 {
    windows
        .get(&id)
        .map_or(1.0, |window| window.window.scale_factor())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_events_report_the_sampled_size_and_scale_not_their_payload() {
        let report = |size: (u32, u32), scale| Report {
            size: PhysicalSize::new(size.0, size.1),
            scale,
            stale: false,
        };
        let actual = (PhysicalSize::new(800, 600), 2.0);
        // A stale snapshot A (the window has since become B): B is reported, never A.
        assert_eq!(
            reported(Change::Resized(PhysicalSize::new(700, 500)), actual),
            Report {
                stale: true,
                ..report((800, 600), 2.0)
            }
        );
        // A current one is reported as it is.
        assert_eq!(
            reported(Change::Resized(PhysicalSize::new(800, 600)), actual),
            report((800, 600), 2.0)
        );
        // A deferred scale change: the size and scale come from the same sample, so the size
        // that went with the new scale is reported with it (not the queued scale alone).
        let moved = (PhysicalSize::new(1600, 1200), 2.0);
        assert_eq!(
            reported(Change::ScaleFactor(1.0), moved),
            Report {
                stale: true,
                ..report((1600, 1200), 2.0)
            }
        );
        assert_eq!(
            reported(Change::ScaleFactor(2.0), moved),
            report((1600, 1200), 2.0)
        );
    }

    #[test]
    fn content_size_requests_are_exact_physical_sizes() {
        // Odd sizes: 777x433 at scale 2 is 388.5x216.5 logical, which `fit` floors to 388x216
        // (776x432 physical), a size that is not the one the source reported.
        let laptop = Some(LogicalSize::new(1512.0, 982.0));
        assert_eq!(
            fit(logical_size(PixelSize::new(777, 433), 2.0), laptop),
            LogicalSize::new(388.0, 216.0)
        );
        assert_eq!(
            content_request(PixelSize::new(777, 433)),
            PhysicalSize::new(777, 433)
        );
        // Beyond the opening fit: on a 1512x982 logical screen at scale 2 it would cap 2800x1600
        // physical to 2720x1554. The request is neither capped nor reshaped.
        let size = PixelSize::new(2800, 1600);
        assert_eq!(
            fit(logical_size(size, 2.0), laptop),
            LogicalSize::new(1360.0, 777.0)
        );
        assert_eq!(content_request(size), PhysicalSize::new(2800, 1600));
    }

    #[test]
    fn proxies_fit_the_screen() {
        let laptop = Some(LogicalSize::new(1512.0, 982.0));
        // An ultrawide source window: scaled down, same shape.
        let fitted = fit(LogicalSize::new(1716.0, 349.0), laptop);
        assert_eq!(fitted, LogicalSize::new(1360.0, 276.0));
        // Small enough already, or no screen known: unchanged.
        assert_eq!(
            fit(LogicalSize::new(320.0, 240.0), laptop),
            LogicalSize::new(320.0, 240.0)
        );
        assert_eq!(
            fit(LogicalSize::new(4000.0, 3000.0), None),
            LogicalSize::new(4000.0, 3000.0)
        );
        // Too tall: limited by height.
        assert_eq!(
            fit(LogicalSize::new(500.0, 2000.0), laptop),
            LogicalSize::new(196.0, 785.0)
        );
    }
}
